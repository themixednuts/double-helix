use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, SystemTime},
};

use helix_core::{
    config::{default_lang_config, user_lang_config},
    syntax::config::Configuration as LanguageConfigurationSet,
};
use helix_pkg::{
    release_age_label, CapabilityCatalog, CapabilityProvider, CapabilityProviderSource,
    CapabilityStatus, ConfiguredCapability, OpEvent, Ops, PackageChange, PackageSpec, PkgKind,
    Receipt, RegistrySource, Source,
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
    finished_revision: u64,
}

impl PkgProgressState {
    pub fn apply(&mut self, event: &OpEvent) {
        self.apply_inner(event);
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
                self.finished_revision = self.finished_revision.saturating_add(1);
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

pub(crate) const ID: &str = "pkg-manager";

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
    const ACP: [Self; 3] = [Self::Browse, Self::Installed, Self::Updates];

    const fn label(self) -> &'static str {
        match self {
            Self::Browse => "Browse",
            Self::Installed => "Installed",
            Self::Updates => "Updates",
            Self::Registries => "Registries",
        }
    }

    fn index_in(self, tabs: &[Self]) -> usize {
        tabs.iter().position(|tab| *tab == self).unwrap_or(0)
    }

    fn from_index_in(index: usize, tabs: &[Self]) -> Option<Self> {
        tabs.get(index).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PkgManagerProfile {
    Packages,
    AcpAgents,
}

impl PkgManagerProfile {
    const fn title(self) -> &'static str {
        match self {
            Self::Packages => "Package Manager",
            Self::AcpAgents => "ACP Agents",
        }
    }

    const fn tabs(self) -> &'static [PkgManagerTab] {
        match self {
            Self::Packages => &PkgManagerTab::ALL,
            Self::AcpAgents => &PkgManagerTab::ACP,
        }
    }

    const fn initial_kind_filter(self) -> Option<PkgKind> {
        match self {
            Self::Packages => None,
            Self::AcpAgents => Some(PkgKind::Acp),
        }
    }

    const fn allows_kind_filter(self) -> bool {
        matches!(self, Self::Packages)
    }

    const fn search_placeholder(self, kind_filter: Option<PkgKind>) -> &'static str {
        match self {
            Self::AcpAgents => "search agents",
            Self::Packages if kind_filter.is_some() => "search filtered packages",
            Self::Packages => "search packages",
        }
    }

    const fn status_legend(self) -> &'static str {
        match self {
            Self::Packages => " ● installed  ○ available  ↑ update  ◍ working  ⚠ problem ",
            Self::AcpAgents => " ● installed  ○ available  ↑ update  ◍ working ",
        }
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
    pub provider: CapabilityProvider,
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
    installable: bool,
    search_blob: String,
}

impl PkgManagerItem {
    fn is_pkg_managed(&self) -> bool {
        self.installed.is_some()
    }

    fn is_usable(&self) -> bool {
        self.provider.is_usable()
    }

    fn is_installable(&self) -> bool {
        self.installable
    }

    fn can_install(&self) -> bool {
        self.is_installable() && (!self.is_pkg_managed() || !self.is_usable())
    }
}

#[derive(Debug, Clone)]
struct RegistryItem {
    name: String,
    source: String,
    cache_age: String,
    doctor: String,
    problem: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PkgManagerData {
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

#[derive(Debug)]
struct PkgRenderConfig {
    rounded_corners: bool,
}

struct PkgRenderSnapshot {
    area: Rect,
    layout: PkgManagerLayout,
    entries: Arc<[PkgManagerItem]>,
    updates: Arc<[PackageChange]>,
    registries: Arc<[RegistryItem]>,
    rows: Arc<[PkgRow]>,
    tabs: Arc<[crate::widgets::Tab<'static>]>,
    title: Arc<str>,
    search_query: Arc<str>,
    search_placeholder: Arc<str>,
    empty_state: &'static str,
    status_legend: &'static str,
    marked: Arc<BTreeSet<String>>,
    accepted_updates: Arc<BTreeSet<String>>,
    progress: Arc<PkgProgressState>,
    theme: Arc<helix_view::Theme>,
    config: Arc<PkgRenderConfig>,
    selection: usize,
    scroll: usize,
    active_tab: usize,
    tab_scroll: u16,
    search_active: bool,
    refresh_active: bool,
    now: SystemTime,
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
    Connect,
    Help,
    Escape,
}

impl PkgManagerProfile {
    const fn allows_action(self, action: PkgAction) -> bool {
        match self {
            Self::Packages => !matches!(action, PkgAction::Connect),
            Self::AcpAgents => matches!(
                action,
                PkgAction::MoveDown
                    | PkgAction::MoveUp
                    | PkgAction::Last
                    | PkgAction::Search
                    | PkgAction::ToggleMark
                    | PkgAction::Tab1
                    | PkgAction::Tab2
                    | PkgAction::Tab3
                    | PkgAction::NextTab
                    | PkgAction::PreviousTab
                    | PkgAction::ToggleDetail
                    | PkgAction::Install
                    | PkgAction::Remove
                    | PkgAction::UpdateSelected
                    | PkgAction::Connect
                    | PkgAction::Help
                    | PkgAction::Escape
            ),
        }
    }
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

    const fn modified(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
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
        BindingKey::new(KeyCode::Tab),
        PkgAction::NextTab,
        Some(("tab/L", "next tab", 250)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('L')),
        PkgAction::NextTab,
        None,
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char(']')),
        PkgAction::NextTab,
        None,
    ),
    PkgBinding::new(
        BindingKey::modified(KeyCode::Tab, KeyModifiers::SHIFT),
        PkgAction::PreviousTab,
        Some(("S-tab/H", "prev tab", 249)),
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('H')),
        PkgAction::PreviousTab,
        None,
    ),
    PkgBinding::new(
        BindingKey::new(KeyCode::Char('[')),
        PkgAction::PreviousTab,
        None,
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
        BindingKey::new(KeyCode::Char('c')),
        PkgAction::Connect,
        Some(("c", "connect agent", 165)),
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
    profile: PkgManagerProfile,
    title: String,
    entries: Arc<[PkgManagerItem]>,
    updates: Arc<[PackageChange]>,
    registries: Arc<[RegistryItem]>,
    rows: Arc<[PkgRow]>,
    tab: PkgManagerTab,
    selection: usize,
    scroll: usize,
    tab_scroll: u16,
    tab_state: crate::widgets::TabsState,
    tab_area: Rect,
    search_query: String,
    search_active: bool,
    kind_filter: Option<PkgKind>,
    marked: Arc<BTreeSet<String>>,
    accepted_updates: Arc<BTreeSet<String>>,
    progress: Arc<PkgProgressState>,
    seen_finished_revision: u64,
    next_refresh_request: u64,
    active_refresh_request: Option<u64>,
    active_refresh_revision: u64,
    pending_refresh_revision: Option<u64>,
    catalog_loaded: bool,
    detail_zoom: bool,
    g_pending: bool,
    shown_help: bool,
    ingress: crate::runtime::RuntimeIngress,
}

pub fn manager(
    editor: &helix_view::Editor,
    ingress: crate::runtime::RuntimeIngress,
) -> anyhow::Result<PkgManager> {
    manager_with_kind(editor, ingress, "Package Manager", None)
}

pub fn acp_manager(
    editor: &helix_view::Editor,
    ingress: crate::runtime::RuntimeIngress,
) -> anyhow::Result<PkgManager> {
    manager_with_kind(editor, ingress, "ACP Agents", Some(PkgKind::Acp))
}

fn manager_with_kind(
    editor: &helix_view::Editor,
    ingress: crate::runtime::RuntimeIngress,
    title: &'static str,
    kind_filter: Option<PkgKind>,
) -> anyhow::Result<PkgManager> {
    let profile = if kind_filter == Some(PkgKind::Acp) {
        PkgManagerProfile::AcpAgents
    } else {
        PkgManagerProfile::Packages
    };
    let mut manager = if profile == PkgManagerProfile::Packages
        && kind_filter.is_none()
        && title == PkgManagerProfile::Packages.title()
    {
        PkgManager::new(PkgManagerData::default(), ingress)
    } else {
        PkgManager::new_with_options(PkgManagerData::default(), ingress, title, kind_filter)
    };
    manager.begin_reload(editor.runtime(), editor.config().pkg.clone(), 0);
    Ok(manager)
}

fn load_catalog_data(ops: &Ops) -> anyhow::Result<PkgManagerData> {
    let receipts = ops.store().receipts()?;
    let runtime_assets = helix_loader::runtime_assets()?;
    let config = user_lang_config().unwrap_or_else(|_| default_lang_config());
    let grammars = helix_loader::grammar::configured_grammar_names()?;
    let mut entries: Vec<_> = CapabilityCatalog::new(ops.registry(), runtime_assets)
        .receipts(receipts)
        .configured(configured_tools(&config, &grammars))
        .statuses()?
        .into_iter()
        .map(|status| item_from_status(status, None))
        .collect();
    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(PkgManagerData {
        entries,
        updates: Vec::new(),
        registries: registry_items(ops, None),
    })
}

fn load_data(ops: &Ops) -> anyhow::Result<PkgManagerData> {
    let receipts = ops.store().receipts()?;
    let doctor = ops.doctor().ok();
    let runtime_assets = helix_loader::runtime_assets()?;

    let config = user_lang_config().unwrap_or_else(|_| default_lang_config());
    let grammars = helix_loader::grammar::configured_grammar_names()?;
    let mut entries: Vec<_> = CapabilityCatalog::new(ops.registry(), runtime_assets)
        .receipts(receipts)
        .configured(configured_tools(&config, &grammars))
        .statuses()?
        .into_iter()
        .map(|status| item_from_status(status, doctor.as_ref()))
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

fn spawn_data_refresh(
    work: helix_runtime::Work,
    block: helix_runtime::Block,
    ingress: crate::runtime::RuntimeIngress,
    config: helix_pkg::PkgConfig,
    request_id: u64,
    finished_revision: u64,
) {
    let result = block.spawn(move || {
        let ops = match Ops::open_with_config(config) {
            Ok(ops) => ops,
            Err(error) => {
                return vec![(
                    crate::runtime::PkgRefreshStage::Catalog,
                    Err(error.to_string()),
                )];
            }
        };

        let catalog = match load_catalog_data(&ops) {
            Ok(data) => data,
            Err(error) => {
                return vec![(
                    crate::runtime::PkgRefreshStage::Catalog,
                    Err(error.to_string()),
                )];
            }
        };

        vec![
            (crate::runtime::PkgRefreshStage::Catalog, Ok(catalog)),
            (
                crate::runtime::PkgRefreshStage::Enrichment,
                load_data(&ops).map_err(|error| error.to_string()),
            ),
        ]
    });
    work.spawn(async move {
        let result = result.await;

        let updates = match result {
            Ok(updates) => updates,
            Err(error) => vec![(
                crate::runtime::PkgRefreshStage::Catalog,
                Err(format!("package data task failed: {error}")),
            )],
        };

        for (stage, result) in updates {
            if ingress
                .send_ui(crate::runtime::UiCommand::Pkg(
                    crate::runtime::PkgCommand::Refresh {
                        request_id,
                        finished_revision,
                        stage,
                        result,
                    },
                ))
                .await
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
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

fn item_from_status(
    status: CapabilityStatus,
    report: Option<&helix_pkg::DoctorReport>,
) -> PkgManagerItem {
    let package = status.package.as_ref();
    let receipt = status.receipt.clone();
    let installed = status
        .active
        .as_ref()
        .map(|package| package.version.clone());
    let latest = package
        .and_then(|package| package.version.tag_source.as_deref())
        .unwrap_or("-")
        .to_owned();
    let aliases = package
        .map(|package| package.aliases.join(", "))
        .or_else(|| configured_aliases(&status))
        .unwrap_or_default();
    let categories = package
        .map(|package| package.categories.join(", "))
        .unwrap_or_else(|| "configured".to_owned());
    let languages = status
        .languages
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let schemas = package
        .map(|package| {
            package
                .schemas
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let source = package
        .map(source_label)
        .unwrap_or_else(|| "languages.toml".to_owned());
    let description = package
        .map(|package| package.description.clone())
        .unwrap_or_else(|| configured_description(&status));
    let homepage = package.and_then(|package| package.homepage.clone());
    let search_blob = search_blob_for_status(&status, &description, &aliases, &categories);
    let problem = match &status.provider {
        CapabilityProvider::BrokenManaged { message, .. } => Some(message.clone()),
        _ => report.and_then(|report| {
            report
                .bad
                .iter()
                .find(|(name, _)| name == &status.name)
                .map(|(_, message)| message.clone())
        }),
    };
    let doctor = if problem.is_some() {
        "problem".to_owned()
    } else if report.is_some_and(|report| report.ok.iter().any(|name| name == &status.name)) {
        "ok".to_owned()
    } else if status.is_pkg_managed() {
        "unknown".to_owned()
    } else if let Some(source) = status.provider.source_label() {
        format!("available via {source}")
    } else if !status.is_installable() {
        "not found".to_owned()
    } else {
        "not installed".to_owned()
    };
    PkgManagerItem {
        name: status.name,
        kind: status.kind,
        installed,
        provider: status.provider,
        latest,
        description,
        homepage,
        aliases,
        categories,
        languages,
        schemas,
        source,
        receipt,
        doctor,
        problem,
        installable: status.installable,
        search_blob,
    }
}

fn configured_aliases(status: &CapabilityStatus) -> Option<String> {
    let aliases = status
        .configured
        .iter()
        .filter_map(|configured| {
            (configured.name != configured.command).then_some(configured.command.as_str())
        })
        .collect::<Vec<_>>();
    (!aliases.is_empty()).then(|| aliases.join(", "))
}

fn configured_description(status: &CapabilityStatus) -> String {
    let command = status
        .configured
        .first()
        .map(|configured| configured.command.as_str())
        .or_else(|| status.provider.command())
        .unwrap_or(status.name.as_str());
    match status.kind {
        PkgKind::Lsp => format!("Configured language server command '{command}'"),
        PkgKind::Dap => format!("Configured debug adapter command '{command}'"),
        PkgKind::Formatter => format!("Configured formatter command '{command}'"),
        _ => format!("Configured command '{command}'"),
    }
}

fn search_blob_for_status(
    status: &CapabilityStatus,
    description: &str,
    aliases: &str,
    categories: &str,
) -> String {
    let mut terms = vec![
        status.name.as_str(),
        status.kind.as_str(),
        description,
        aliases,
        categories,
    ];
    if status.package.is_none() {
        terms.push("configured");
        terms.push("languages.toml");
    }
    for configured in &status.configured {
        terms.push(configured.command.as_str());
    }
    let languages = status.languages.iter().map(String::as_str);
    terms.extend(languages);
    if let Some(package) = &status.package {
        terms.extend(package.search_terms());
    }
    terms.join("\n")
}

fn configured_tools(
    config: &LanguageConfigurationSet,
    configured_grammars: &BTreeSet<String>,
) -> Vec<ConfiguredCapability> {
    let mut tools: BTreeMap<(PkgKind, String), (String, BTreeSet<String>)> = BTreeMap::new();
    for language in &config.language {
        let grammar = language.grammar.as_deref().unwrap_or(&language.language_id);
        if configured_grammars.contains(grammar) {
            record_configured_tool(
                &mut tools,
                PkgKind::Grammar,
                grammar,
                grammar,
                &language.language_id,
            );
        }
        for server in &language.language_servers {
            let Some(server_config) = config.language_server.get(&server.name) else {
                continue;
            };
            record_configured_tool(
                &mut tools,
                PkgKind::Lsp,
                &server.name,
                &server_config.command,
                &language.language_id,
            );
        }
        if let Some(debugger) = &language.debugger {
            let name = if debugger.name.trim().is_empty() {
                debugger.command.as_str()
            } else {
                debugger.name.as_str()
            };
            record_configured_tool(
                &mut tools,
                PkgKind::Dap,
                name,
                &debugger.command,
                &language.language_id,
            );
        }
        if let Some(formatter) = &language.formatter {
            record_configured_tool(
                &mut tools,
                PkgKind::Formatter,
                &formatter.command,
                &formatter.command,
                &language.language_id,
            );
        }
    }

    tools
        .into_iter()
        .filter_map(|((kind, name), (command, languages))| {
            ConfiguredCapability::new(kind, name, command, languages)
        })
        .collect()
}

fn record_configured_tool(
    tools: &mut BTreeMap<(PkgKind, String), (String, BTreeSet<String>)>,
    kind: PkgKind,
    name: &str,
    command: &str,
    language: &str,
) {
    let name = name.trim();
    let command = command.trim();
    if name.is_empty() || command.is_empty() {
        return;
    }
    let (_, languages) = tools
        .entry((kind, name.to_owned()))
        .or_insert_with(|| (command.to_owned(), BTreeSet::new()));
    languages.insert(language.to_owned());
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
        Self::new_with_profile(
            data,
            ingress,
            PkgManagerProfile::Packages.title(),
            PkgManagerProfile::Packages,
            None,
        )
    }

    fn new_with_options(
        data: PkgManagerData,
        ingress: crate::runtime::RuntimeIngress,
        title: &'static str,
        kind_filter: Option<PkgKind>,
    ) -> Self {
        let profile = if kind_filter == Some(PkgKind::Acp) {
            PkgManagerProfile::AcpAgents
        } else {
            PkgManagerProfile::Packages
        };
        Self::new_with_profile(data, ingress, title, profile, kind_filter)
    }

    fn new_with_profile(
        data: PkgManagerData,
        ingress: crate::runtime::RuntimeIngress,
        title: &'static str,
        profile: PkgManagerProfile,
        kind_filter: Option<PkgKind>,
    ) -> Self {
        let catalog_loaded =
            !data.entries.is_empty() || !data.updates.is_empty() || !data.registries.is_empty();
        let mut manager = Self {
            profile,
            title: title.to_owned(),
            entries: data.entries.into(),
            updates: data.updates.into(),
            registries: data.registries.into(),
            rows: Arc::from([]),
            tab: PkgManagerTab::Browse,
            selection: 0,
            scroll: 0,
            tab_scroll: 0,
            tab_state: crate::widgets::TabsState::default(),
            tab_area: Rect::default(),
            search_query: String::new(),
            search_active: false,
            kind_filter: kind_filter.or_else(|| profile.initial_kind_filter()),
            marked: Arc::new(BTreeSet::new()),
            accepted_updates: Arc::new(BTreeSet::new()),
            progress: Arc::new(PkgProgressState::default()),
            seen_finished_revision: 0,
            next_refresh_request: 0,
            active_refresh_request: None,
            active_refresh_revision: 0,
            pending_refresh_revision: None,
            catalog_loaded,
            detail_zoom: false,
            g_pending: false,
            shown_help: false,
            ingress,
        };
        manager.accepted_updates = Arc::new(
            manager
                .updates
                .iter()
                .filter(|change| change.needs_apply())
                .map(|change| change.name.clone())
                .collect(),
        );
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
        let rows = match self.tab {
            PkgManagerTab::Browse | PkgManagerTab::Installed => self.package_rows(&query),
            PkgManagerTab::Updates => self.update_rows(&query),
            PkgManagerTab::Registries => (0..self.registries.len()).map(PkgRow::Registry).collect(),
        };
        self.rows = rows.into();

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
            if self.tab == PkgManagerTab::Installed && !item.is_usable() && !item.is_pkg_managed() {
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
        if !self.profile.tabs().contains(&tab) {
            return;
        }
        self.tab = tab;
        self.selection = 0;
        self.scroll = 0;
        self.g_pending = false;
        self.rebuild_filter();
    }

    fn set_tab_index(&mut self, index: usize) {
        if let Some(tab) = PkgManagerTab::from_index_in(index, self.profile.tabs()) {
            self.set_tab(tab);
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        let tabs = self.profile.tabs();
        let current = self.tab.index_in(tabs) as isize;
        let len = tabs.len() as isize;
        let next = (current + delta).rem_euclid(len) as usize;
        self.set_tab(tabs[next]);
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
        if let Err(error) = self
            .ingress
            .package(operation, cx.editor.config().pkg.clone())
        {
            cx.editor.notify_error(error.to_string());
        }
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

    fn selected_items_for_op(&self) -> Vec<&PkgManagerItem> {
        if !self.marked.is_empty() {
            return self
                .marked
                .iter()
                .filter_map(|name| self.entries.iter().find(|item| item.name == *name))
                .collect();
        }
        self.selected_item().into_iter().collect()
    }

    fn install_selected(&mut self, cx: &mut Context) {
        let selected_items = self.selected_items_for_op();
        if selected_items.is_empty() {
            cx.editor.notify_warning("No package selected");
            return;
        }

        let skipped_uninstallable = selected_items
            .iter()
            .filter(|item| !item.is_installable())
            .count();
        let names = selected_items
            .iter()
            .filter(|item| item.can_install())
            .map(|item| item.name.clone())
            .collect::<Vec<_>>();
        if !names.is_empty() {
            for name in &names {
                Arc::make_mut(&mut self.marked).remove(name);
            }
            let label = names.join(", ");
            cx.editor.notify_info(format!("Installing {label}"));
            self.start_operation(cx, crate::runtime::PkgOperation::Install(names));
            if skipped_uninstallable > 0 {
                let noun = if skipped_uninstallable == 1 {
                    "row"
                } else {
                    "rows"
                };
                cx.editor.notify_info(format!(
                    "Skipped {skipped_uninstallable} configured {noun} without package recipes"
                ));
            }
        } else if skipped_uninstallable > 0 {
            cx.editor
                .notify_warning("No package recipe for selected configured tool");
        } else {
            cx.editor
                .notify_info("Selected package is already installed");
        }
    }

    fn activate_selected(&mut self, cx: &mut Context) -> Option<PostAction> {
        if self.profile == PkgManagerProfile::AcpAgents {
            if let Some(PkgRow::Update(index)) = self.selected_row() {
                let Some(change) = self.updates.get(*index) else {
                    cx.editor.notify_warning("No agent update selected");
                    return None;
                };
                if change.needs_apply() {
                    let name = change.name.clone();
                    cx.editor.notify_info(format!("Updating {name}"));
                    self.start_operation(cx, crate::runtime::PkgOperation::Update(vec![name]));
                } else {
                    cx.editor.notify_info("Selected agent is already current");
                }
                return None;
            }

            if self
                .selected_item()
                .is_some_and(|item| item.kind == PkgKind::Acp && item.is_usable())
            {
                return self.connect_selected_agent(cx);
            }
        }

        self.install_selected(cx);
        None
    }

    fn update_selected(&mut self, cx: &mut Context) {
        if self.tab == PkgManagerTab::Updates {
            self.reload(cx);
            return;
        }
        let names = self.selected_names_for_op();
        if !names.is_empty() {
            let managed = self.pkg_managed_names(names);
            if managed.is_empty() {
                cx.editor.notify_warning("No pkg-managed package selected");
            } else {
                self.start_operation(cx, crate::runtime::PkgOperation::Update(managed));
            }
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
        let managed = self.pkg_managed_names(self.selected_names_for_op());
        if managed.is_empty() {
            cx.editor.notify_warning("No pkg-managed package selected");
            return;
        }
        for name in managed {
            self.start_operation(cx, crate::runtime::PkgOperation::Rollback(name));
        }
    }

    fn doctor_selected(&self, cx: &mut Context) {
        let names = self.selected_names_for_op();
        if names.is_empty() {
            self.start_operation(cx, crate::runtime::PkgOperation::Doctor);
        } else {
            let selected_count = names.len();
            let managed = self.pkg_managed_names(names);
            if managed.is_empty() {
                cx.editor.notify_info(
                    "Selected package is available externally; use --health for resolver diagnostics",
                );
                return;
            }
            let skipped = selected_count.saturating_sub(managed.len());
            if skipped > 0 {
                let noun = if skipped == 1 { "package" } else { "packages" };
                cx.editor.notify_info(format!(
                    "Skipping {skipped} selected {noun} that are not pkg-managed"
                ));
            }
            for name in managed {
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

    fn connect_selected_agent(&self, cx: &mut Context) -> Option<PostAction> {
        let Some(item) = self.selected_item() else {
            cx.editor.notify_warning("No package selected");
            return None;
        };
        if item.kind != PkgKind::Acp {
            cx.editor
                .notify_warning("Select an installed ACP agent package to connect");
            return None;
        }
        if !item.is_usable() {
            cx.editor
                .notify_warning(format!("Install {} with pkg before connecting", item.name));
            return None;
        }

        let package_name = item.name.clone();
        match cx.editor.assistant_acp_package_agent(&package_name) {
            Ok(Some(agent)) => {
                let agent_name = agent.name.clone();
                let connection = match crate::runtime::AssistantBackendConnection::from_agent(
                    agent,
                    None,
                    helix_view::editor::PanelBehavior::Open,
                ) {
                    Ok(connection) => connection,
                    Err(err) => {
                        cx.editor.notify_error(format!(
                            "Failed to connect assistant agent {agent_name}: {err}"
                        ));
                        return None;
                    }
                };
                cx.submit_task(crate::runtime::RuntimeTaskEvent::ConnectAssistantBackend(
                    Box::new(connection),
                ));
                cx.editor
                    .notify_info(format!("Connecting assistant agent {agent_name}"));
                Some(PostAction::PopLayer {
                    model_layer: None,
                    remember_picker: false,
                })
            }
            Ok(None) => {
                cx.editor.notify_warning(format!(
                    "{} is not installed for this platform",
                    package_name
                ));
                None
            }
            Err(err) => {
                cx.editor.notify_error(format!(
                    "Failed to load assistant agent {}: {err}",
                    package_name
                ));
                None
            }
        }
    }

    fn remove_selected(&mut self, cx: &mut Context) -> EventResult {
        let names = self.selected_names_for_op();
        let selected_count = names.len();
        let installed = names
            .into_iter()
            .filter(|name| {
                self.entries
                    .iter()
                    .any(|item| item.name == *name && item.is_pkg_managed())
            })
            .collect::<Vec<_>>();
        if installed.is_empty() {
            cx.editor.notify_warning("No pkg-managed package selected");
            return EventResult::Consumed(None);
        }

        let skipped = selected_count.saturating_sub(installed.len());
        let label = remove_confirmation_label(&installed, skipped);
        cx.editor
            .notify_warning(format!("Package manager: {label} Type y to confirm."));
        for name in &installed {
            Arc::make_mut(&mut self.marked).remove(name);
        }
        let ingress = self.ingress.clone();
        let confirmation = crate::ui::Confirmation::new(label, move |cx| {
            if let Err(error) = ingress.package(
                crate::runtime::PkgOperation::Remove(installed.clone()),
                cx.editor.config().pkg.clone(),
            ) {
                cx.editor.notify_error(error.to_string());
            }
        });
        EventResult::Consumed(Some(confirmation.into_post_action()))
    }

    fn pkg_managed_names(&self, names: Vec<String>) -> Vec<String> {
        names
            .into_iter()
            .filter(|name| {
                self.entries
                    .iter()
                    .any(|item| item.name == *name && item.is_pkg_managed())
            })
            .collect()
    }

    fn reload(&mut self, cx: &mut Context) {
        self.begin_reload(
            cx.editor.runtime(),
            cx.editor.config().pkg.clone(),
            self.seen_finished_revision,
        );
        cx.editor.set_status("Refreshing package data");
    }

    pub(crate) fn apply_progress_event(&mut self, event: &OpEvent) {
        Arc::make_mut(&mut self.progress).apply(event);
    }

    pub(crate) fn apply_operation_outcome(
        &mut self,
        editor: &helix_view::Editor,
        outcome: &crate::runtime::PkgOperationOutcome,
    ) {
        if !outcome.is_success() && !outcome.runtime_changed {
            return;
        }
        let latest_revision = self
            .pending_refresh_revision
            .unwrap_or_default()
            .max(self.active_refresh_revision)
            .max(self.seen_finished_revision);
        self.begin_reload(
            editor.runtime(),
            editor.config().pkg.clone(),
            latest_revision.saturating_add(1),
        );
    }

    fn begin_reload(
        &mut self,
        runtime: &helix_runtime::Runtime,
        config: helix_pkg::PkgConfig,
        finished_revision: u64,
    ) {
        if self.active_refresh_request.is_some() {
            self.pending_refresh_revision = Some(
                self.pending_refresh_revision
                    .map_or(finished_revision, |pending| pending.max(finished_revision)),
            );
            return;
        }

        self.next_refresh_request = self.next_refresh_request.wrapping_add(1).max(1);
        let request_id = self.next_refresh_request;
        self.active_refresh_request = Some(request_id);
        self.active_refresh_revision = finished_revision;
        spawn_data_refresh(
            runtime.work().clone(),
            runtime.block().clone(),
            self.ingress.clone(),
            config,
            request_id,
            finished_revision,
        );
    }

    fn replace_data(&mut self, data: PkgManagerData) {
        self.entries = data.entries.into();
        self.updates = data.updates.into();
        self.registries = data.registries.into();
        self.accepted_updates = Arc::new(
            self.updates
                .iter()
                .filter(|change| change.needs_apply())
                .map(|change| change.name.clone())
                .collect(),
        );
        self.catalog_loaded = true;
        self.rebuild_filter();
    }

    fn finish_reload(&mut self, editor: &helix_view::Editor, finished_revision: u64) {
        self.active_refresh_request = None;
        self.seen_finished_revision = self.seen_finished_revision.max(finished_revision);
        if let Some(pending_revision) = self.pending_refresh_revision.take() {
            self.begin_reload(
                editor.runtime(),
                editor.config().pkg.clone(),
                pending_revision,
            );
        }
    }

    pub(crate) fn apply_refresh_result(
        &mut self,
        editor: &mut helix_view::Editor,
        request_id: u64,
        finished_revision: u64,
        stage: crate::runtime::PkgRefreshStage,
        result: Result<PkgManagerData, String>,
    ) {
        if self.active_refresh_request != Some(request_id) {
            return;
        }

        match (stage, result) {
            (crate::runtime::PkgRefreshStage::Catalog, Ok(data)) => {
                self.replace_data(data);
            }
            (crate::runtime::PkgRefreshStage::Catalog, Err(err)) => {
                editor.notify_error(format!("Failed to refresh package data: {err}"));
                self.finish_reload(editor, finished_revision);
            }
            (crate::runtime::PkgRefreshStage::Enrichment, Ok(data)) => {
                self.replace_data(data);
                self.finish_reload(editor, finished_revision);
            }
            (crate::runtime::PkgRefreshStage::Enrichment, Err(err)) => {
                editor.notify_warning(format!("Package checks did not finish: {err}"));
                self.finish_reload(editor, finished_revision);
            }
        }
    }

    fn toggle_mark(&mut self) {
        match self.selected_row() {
            Some(PkgRow::Package(index)) => {
                let name = self.entries[*index].name.clone();
                let marked = Arc::make_mut(&mut self.marked);
                if !marked.remove(&name) {
                    marked.insert(name);
                }
            }
            Some(PkgRow::Update(index)) => {
                let name = self.updates[*index].name.clone();
                let accepted_updates = Arc::make_mut(&mut self.accepted_updates);
                if !accepted_updates.remove(&name) {
                    accepted_updates.insert(name);
                }
            }
            _ => {}
        }
    }

    fn cycle_kind_filter(&mut self) {
        if !self.profile.allows_kind_filter() {
            return;
        }
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

    fn binding_for_key(&self, key: &KeyEvent) -> Option<&'static PkgBinding> {
        PKG_BINDINGS
            .iter()
            .find(|binding| binding.key.matches(key) && self.profile.allows_action(binding.action))
    }

    fn help_hint(&self, binding: &PkgBinding) -> Option<(&'static str, &'static str, u16)> {
        let (key, label, rank) = binding.hint?;
        if matches!(
            binding.action,
            PkgAction::MoveDown
                | PkgAction::MoveUp
                | PkgAction::Last
                | PkgAction::ToggleMark
                | PkgAction::Tab1
                | PkgAction::NextTab
                | PkgAction::PreviousTab
        ) {
            return None;
        }
        if self.profile == PkgManagerProfile::AcpAgents {
            return match binding.action {
                PkgAction::Install => Some(("enter", "install / connect", rank)),
                PkgAction::UpdateSelected => Some((key, "refresh / update", rank)),
                _ => Some((key, label, rank)),
            };
        }
        Some((key, label, rank))
    }

    fn help_entries(&self) -> Vec<(&'static str, &'static str)> {
        let mut entries = PKG_BINDINGS
            .iter()
            .filter(|binding| self.profile.allows_action(binding.action))
            .filter_map(|binding| self.help_hint(binding))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.2));
        entries
            .into_iter()
            .map(|(key, label, _)| (key, label))
            .collect()
    }

    fn info(&self) -> helix_view::info::Info {
        helix_view::info::Info::new(self.profile.title(), &self.help_entries())
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

    fn prepare_render_snapshot(&mut self, area: Rect, cx: &RenderContext) -> PkgRenderSnapshot {
        let layout = Self::layout(area, self.detail_zoom);
        let update_count = self
            .updates
            .iter()
            .filter(|change| change.needs_apply())
            .count();
        let tabs: Arc<[crate::widgets::Tab<'static>]> =
            package_tabs(self.profile, update_count).into();
        let active_tab = self.tab.index_in(self.profile.tabs());
        let tab_options = crate::widgets::TabsOptions::new(active_tab)
            .scroll(self.tab_scroll)
            .separator(" ");
        let tab_state =
            crate::widgets::tabs_layout_with_options(&tabs, layout.tabs.width, &tab_options);
        self.tab_scroll = tab_state.scroll;
        self.tab_state = tab_state;
        self.tab_area = layout.tabs;
        self.scroll = clamp_list_scroll(
            layout.list,
            self.rows.len(),
            (!self.rows.is_empty()).then_some(self.selection),
            self.scroll,
        );

        let search_placeholder = self
            .kind_filter
            .filter(|_| self.profile.allows_kind_filter())
            .map_or_else(
                || self.profile.search_placeholder(self.kind_filter).to_owned(),
                |kind| format!("kind:{kind}"),
            );

        PkgRenderSnapshot {
            area,
            layout,
            entries: Arc::clone(&self.entries),
            updates: Arc::clone(&self.updates),
            registries: Arc::clone(&self.registries),
            rows: Arc::clone(&self.rows),
            tabs,
            title: Arc::from(self.title.as_str()),
            search_query: Arc::from(self.search_query.as_str()),
            search_placeholder: Arc::from(search_placeholder),
            empty_state: self.empty_state(),
            status_legend: self.profile.status_legend(),
            marked: Arc::clone(&self.marked),
            accepted_updates: Arc::clone(&self.accepted_updates),
            progress: Arc::clone(&self.progress),
            theme: cx.theme_arc(),
            config: Arc::new(PkgRenderConfig {
                rounded_corners: cx.config().rounded_corners,
            }),
            selection: self.selection,
            scroll: self.scroll,
            active_tab,
            tab_scroll: self.tab_scroll,
            search_active: self.search_active,
            refresh_active: self.active_refresh_request.is_some(),
            now: SystemTime::now(),
        }
    }

    fn empty_state(&self) -> &'static str {
        if !self.catalog_loaded {
            return "loading package catalog";
        }
        if self.kind_filter == Some(PkgKind::Acp) {
            return match self.tab {
                PkgManagerTab::Browse => "no ACP agents match the filter",
                PkgManagerTab::Installed => "no installed ACP agents",
                PkgManagerTab::Updates => "no ACP agent updates - u to refresh",
                PkgManagerTab::Registries => "no registry sources configured",
            };
        }
        match self.tab {
            PkgManagerTab::Browse => "no packages match the filter",
            PkgManagerTab::Installed => "no installed packages",
            PkgManagerTab::Updates => "no updates - u to refresh",
            PkgManagerTab::Registries => "no registry sources configured",
        }
    }

    #[cfg(feature = "storybook")]
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

fn package_tabs(
    profile: PkgManagerProfile,
    update_count: usize,
) -> Vec<crate::widgets::Tab<'static>> {
    profile
        .tabs()
        .iter()
        .copied()
        .map(|tab| {
            let model = crate::widgets::Tab::new(tab.label());
            if tab == PkgManagerTab::Updates {
                model.badge(update_count.to_string())
            } else {
                model
            }
        })
        .collect()
}

fn clamp_list_scroll(
    area: Rect,
    item_count: usize,
    selection: Option<usize>,
    scroll: usize,
) -> usize {
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    helix_view::list_nav::ListViewport::new(item_count, selection, area.height as usize, scroll)
        .selected_visible_range()
        .start
}

impl PkgRenderSnapshot {
    fn render(
        self,
        cancellation: &crate::render::RenderCancellation,
    ) -> crate::render::RenderOutput {
        let mut output = crate::render::RenderOutput::sparse(self.area);
        paint_pkg_manager(output.surface_mut(), &self, cancellation);
        output
    }
}

fn paint_pkg_manager(
    surface: &mut crate::render::CellSurface,
    snapshot: &PkgRenderSnapshot,
    cancellation: &crate::render::RenderCancellation,
) {
    if cancellation.is_cancelled() || snapshot.area.width == 0 || snapshot.area.height == 0 {
        return;
    }

    let styles = PkgManagerStyles::from_theme(&snapshot.theme);
    let panel_style =
        crate::widgets::PanelStyle::new(styles.background, styles.border, styles.title);
    let title = format!(" {} ", snapshot.title);
    crate::widgets::Panel::framed(panel_style, snapshot.config.rounded_corners)
        .title(&title)
        .render(surface, snapshot.area);

    if cancellation.is_cancelled() {
        return;
    }
    paint_pkg_tabs(surface, snapshot, styles);
    if cancellation.is_cancelled() {
        return;
    }
    paint_pkg_search(surface, snapshot, styles);
    if cancellation.is_cancelled() {
        return;
    }
    paint_pkg_list(surface, snapshot, styles, cancellation);
    if cancellation.is_cancelled() {
        return;
    }
    paint_pkg_detail(surface, snapshot, styles, cancellation);
    if cancellation.is_cancelled() {
        return;
    }
    paint_pkg_status(surface, snapshot, styles);
}

fn paint_pkg_tabs(
    surface: &mut crate::render::CellSurface,
    snapshot: &PkgRenderSnapshot,
    styles: PkgManagerStyles,
) {
    let state = crate::widgets::tabs_with_options(
        surface,
        snapshot.layout.tabs,
        &snapshot.tabs,
        crate::widgets::TabsOptions::new(snapshot.active_tab)
            .scroll(snapshot.tab_scroll)
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
    debug_assert_eq!(state.scroll, snapshot.tab_scroll);
}

fn paint_pkg_search(
    surface: &mut crate::render::CellSurface,
    snapshot: &PkgRenderSnapshot,
    styles: PkgManagerStyles,
) {
    let area = snapshot.layout.search;
    let input_area = snapshot.layout.search_input;
    surface.set_style(
        tui::ratatui::to_ratatui_rect(area),
        tui::ratatui::to_ratatui_style(styles.background),
    );
    let marker_style = if snapshot.search_active {
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
    if snapshot.search_query.is_empty() && !snapshot.search_active {
        surface.set_stringn(
            input_area.x,
            input_area.y,
            &snapshot.search_placeholder,
            input_area.width as usize,
            tui::ratatui::to_ratatui_style(styles.inactive),
        );
    } else {
        crate::widgets::text_input(
            surface,
            input_area,
            &snapshot.search_query,
            snapshot.search_query.len(),
            styles.text,
            styles.selection,
        );
    }
}

fn paint_pkg_list(
    surface: &mut crate::render::CellSurface,
    snapshot: &PkgRenderSnapshot,
    styles: PkgManagerStyles,
    cancellation: &crate::render::RenderCancellation,
) {
    let list_styles = crate::widgets::ListStyles {
        normal: styles.background,
        selected: styles.selection,
        scrollbar_thumb: styles.inactive,
        scrollbar_track: styles.background,
    };
    let sticky_rows = snapshot
        .rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| matches!(row, PkgRow::Section(_)).then_some(index))
        .collect::<Vec<_>>();
    let state = crate::widgets::item_list_with_marks_and_sticky(
        surface,
        snapshot.layout.list,
        snapshot.rows.len(),
        (!snapshot.rows.is_empty()).then_some(snapshot.selection),
        snapshot.scroll,
        None,
        Some(crate::widgets::StickyRows::new(&sticky_rows)),
        &list_styles,
        |row_index, row_area, surface, selected, _marked| {
            if !cancellation.is_cancelled() {
                paint_pkg_row(surface, row_area, row_index, selected, snapshot, styles);
            }
        },
    );
    debug_assert_eq!(state.scroll, snapshot.scroll);
    if snapshot.rows.is_empty() && !cancellation.is_cancelled() {
        render_empty_state(surface, snapshot.layout.list, snapshot.empty_state, styles);
    }
}

fn paint_pkg_row(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    row_index: usize,
    selected: bool,
    snapshot: &PkgRenderSnapshot,
    styles: PkgManagerStyles,
) {
    let Some(row) = snapshot.rows.get(row_index) else {
        return;
    };
    match row {
        PkgRow::Section(kind) => render_section_row(surface, area, *kind, styles),
        PkgRow::Package(index) => {
            let Some(item) = snapshot.entries.get(*index) else {
                return;
            };
            let progress = snapshot.progress.active.get(&item.name);
            render_package_row(
                surface,
                area,
                item,
                progress,
                selected,
                snapshot.marked.contains(&item.name),
                styles,
            );
        }
        PkgRow::Update(index) => {
            let Some(change) = snapshot.updates.get(*index) else {
                return;
            };
            render_update_row(
                surface,
                area,
                change,
                selected,
                snapshot.accepted_updates.contains(&change.name),
                styles,
            );
        }
        PkgRow::Registry(index) => {
            if let Some(registry) = snapshot.registries.get(*index) {
                render_registry_row(surface, area, registry, selected, styles);
            }
        }
    }
}

fn paint_pkg_detail(
    surface: &mut crate::render::CellSurface,
    snapshot: &PkgRenderSnapshot,
    styles: PkgManagerStyles,
    cancellation: &crate::render::RenderCancellation,
) {
    let area = snapshot.layout.detail;
    if area.width == 0 || area.height == 0 {
        return;
    }
    surface.set_style(
        tui::ratatui::to_ratatui_rect(area),
        tui::ratatui::to_ratatui_style(styles.background),
    );
    match snapshot.rows.get(snapshot.selection) {
        Some(PkgRow::Package(index)) => {
            if let Some(item) = snapshot.entries.get(*index) {
                render_package_detail(surface, area, item, styles);
            }
        }
        Some(PkgRow::Update(index)) => {
            if let Some(change) = snapshot.updates.get(*index) {
                render_update_detail(
                    surface,
                    area,
                    change,
                    snapshot.accepted_updates.contains(&change.name),
                    styles,
                    snapshot.now,
                    cancellation,
                );
            }
        }
        Some(PkgRow::Registry(index)) => {
            if let Some(registry) = snapshot.registries.get(*index) {
                render_registry_detail(surface, area, registry, styles);
            }
        }
        _ => render_empty_state(surface, area, snapshot.empty_state, styles),
    }
}

fn paint_pkg_status(
    surface: &mut crate::render::CellSurface,
    snapshot: &PkgRenderSnapshot,
    styles: PkgManagerStyles,
) {
    let area = snapshot.layout.status;
    surface.set_style(
        tui::ratatui::to_ratatui_rect(area),
        tui::ratatui::to_ratatui_style(styles.title),
    );
    let Some(status) = snapshot.progress.statusline() else {
        if snapshot.refresh_active {
            surface.set_stringn(
                area.x,
                area.y,
                " ◍ Loading package data ",
                area.width as usize,
                tui::ratatui::to_ratatui_style(styles.warning),
            );
            return;
        }
        surface.set_stringn(
            area.x,
            area.y,
            snapshot.status_legend,
            area.width as usize,
            tui::ratatui::to_ratatui_style(styles.inactive),
        );
        return;
    };
    let queue = snapshot.progress.active_count().saturating_sub(1);
    let label = if queue > 0 {
        format!("{} ({queue} queued)", status.label)
    } else {
        status.label
    };
    let label = status.percent.map_or_else(
        || format!(" ◍ {label} "),
        |percent| format!(" ◍ {label} {percent:>3}% "),
    );
    surface.set_stringn(
        area.x,
        area.y,
        &label,
        area.width as usize,
        tui::ratatui::to_ratatui_style(styles.warning),
    );
}

#[cfg(feature = "storybook")]
fn story_item(
    name: &str,
    kind: PkgKind,
    receipt: Option<Receipt>,
    languages: &str,
) -> PkgManagerItem {
    let installed = receipt.as_ref().map(|receipt| receipt.version.clone());
    let provider = installed
        .as_ref()
        .map(|version| CapabilityProvider::Managed {
            version: version.clone(),
        })
        .unwrap_or(CapabilityProvider::Missing);
    PkgManagerItem {
        name: name.to_owned(),
        kind,
        installed,
        provider,
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
        installable: true,
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
    } else if item.is_usable() {
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
        PkgKind::Acp => "ACP agents",
    }
}

fn render_package_row(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    item: &PkgManagerItem,
    progress: Option<&PkgStatusline>,
    selected: bool,
    marked: bool,
    styles: PkgManagerStyles,
) {
    let state = row_state(item, progress, item.problem.as_deref());
    let status = progress
        .and_then(|progress| {
            progress
                .percent
                .map(|percent| format!("installing {percent}%"))
        })
        .unwrap_or_else(|| status_label(item));
    let version = item
        .installed
        .as_deref()
        .or_else(|| item.provider.command())
        .unwrap_or(&item.latest);
    render_columns(
        surface,
        area,
        state,
        &item.name,
        version,
        item.problem.as_deref().unwrap_or(&status),
        &item.languages,
        selected,
        marked,
        styles,
    );
}

fn render_update_row(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    change: &PackageChange,
    selected: bool,
    marked: bool,
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
        marked,
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
        false,
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
    marked: bool,
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
    let glyph_style = row_glyph_style(styles, state, marked);
    surface.set_stringn(
        area.x,
        area.y,
        row_glyph(state, marked),
        1,
        tui::ratatui::to_ratatui_style(glyph_style),
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

fn status_label(item: &PkgManagerItem) -> String {
    match &item.provider {
        CapabilityProvider::Managed { version } => format!("installed {version}"),
        CapabilityProvider::Command { source, .. } => match source {
            CapabilityProviderSource::Path => "on PATH".to_owned(),
            other => other.label().to_owned(),
        },
        CapabilityProvider::RuntimeGrammar { .. } => {
            CapabilityProviderSource::Runtime.label().to_owned()
        }
        CapabilityProvider::BrokenManaged { version, .. } => {
            format!("broken pkg {version}")
        }
        CapabilityProvider::Missing => {
            if !item.is_installable() {
                "unpackaged".to_owned()
            } else {
                "available".to_owned()
            }
        }
    }
}

fn remove_confirmation_label(names: &[String], skipped: usize) -> String {
    let mut label = if names.len() == 1 {
        format!("Remove package {}?", names[0])
    } else {
        format!("Remove {} selected packages?", names.len())
    };
    if skipped > 0 {
        let noun = if skipped == 1 { "package" } else { "packages" };
        label.push_str(&format!(" ({skipped} selected {noun} not pkg-managed)"));
    }
    label
}

const fn row_glyph(state: RowState, marked: bool) -> &'static str {
    if marked && !matches!(state, RowState::Working | RowState::Problem) {
        "●"
    } else {
        state.glyph()
    }
}

fn row_glyph_style(styles: PkgManagerStyles, state: RowState, marked: bool) -> Style {
    if marked && !matches!(state, RowState::Working | RowState::Problem) {
        styles.warning
    } else {
        styles.row_state(state, false)
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
    let receipt_line = if let Some(receipt) = receipt {
        let verified = if receipt.archive_sha256.is_empty() {
            "sha256 -"
        } else {
            "sha256 ok"
        };
        format!(
            "installed {} ({}, {})",
            receipt.version, receipt.source, verified
        )
    } else if let CapabilityProvider::Command {
        command,
        path,
        source,
    } = &item.provider
    {
        format!(
            "usable via {}: {} ({})",
            source.label(),
            command,
            path.display()
        )
    } else if let CapabilityProvider::RuntimeGrammar { grammar } = &item.provider {
        format!("usable via runtime: {grammar} (runtime grammar)")
    } else if !item.is_installable() {
        "configured in languages.toml; no package recipe".to_owned()
    } else {
        "not installed".to_owned()
    };
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
    if item.kind == PkgKind::Acp {
        let action = if item.is_usable() {
            "action: enter/c connect"
        } else {
            "action: enter/i install"
        };
        write_line(surface, area, &mut y, action, styles.text);
    }
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
    now: SystemTime,
    cancellation: &crate::render::RenderCancellation,
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
        let release = release_age_label(candidate.published_at.as_deref(), &candidate.version, now);
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
        if cancellation.is_cancelled() {
            return;
        }
        write_text_layout(
            surface,
            area,
            &mut y,
            &format!("warning: {warning}"),
            styles.warning,
        );
    }
    if cancellation.is_cancelled() {
        return;
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
    fn prepare_render(&mut self, area: Rect, cx: &RenderContext) -> crate::render::PreparedRender {
        let snapshot = self.prepare_render_snapshot(area, cx);
        crate::render::PreparedRender::deferred(move |cancellation| snapshot.render(cancellation))
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
            Event::IdleTimeout | Event::FocusGained => return EventResult::Ignored(None),
            Event::FocusLost => {
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

        let Some(binding) = self.binding_for_key(&key) else {
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
            PkgAction::Install => {
                if let Some(action) = self.activate_selected(cx) {
                    return EventResult::Consumed(Some(action));
                }
            }
            PkgAction::Remove => return self.remove_selected(cx),
            PkgAction::UpdateSelected => self.update_selected(cx),
            PkgAction::ApplyUpdates => self.apply_updates(cx),
            PkgAction::Rollback => self.rollback_selected(cx),
            PkgAction::Doctor => self.doctor_selected(cx),
            PkgAction::RegistryUpdate => self.registry_update_selected(cx),
            PkgAction::Connect => {
                if let Some(action) = self.connect_selected_agent(cx) {
                    return EventResult::Consumed(Some(action));
                }
            }
            PkgAction::Help => self.toggle_help(cx.editor),
            PkgAction::Escape => {
                self.clear_help(cx.editor);
                if self.search_active {
                    self.search_active = false;
                } else if !self.search_query.is_empty() {
                    self.search_query.clear();
                    self.rebuild_filter();
                } else if !self.marked.is_empty() {
                    Arc::make_mut(&mut self.marked).clear();
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
    fn pkg_render_snapshot_and_prepared_render_are_send() {
        fn assert_send<T: Send>() {}

        assert_send::<PkgRenderSnapshot>();
        assert_send::<crate::render::PreparedRender>();
    }

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
            let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());

            let _manager = manager(&editor, ingress).expect("pkg manager opens");
        });
    }

    #[test]
    fn prepare_render_updates_interaction_state_without_changing_cursor() {
        let area = Rect::new(0, 0, 18, 24);
        let runtime = helix_runtime::test::runtime();
        let editor = helix_view::editor::EditorBuilder::new(area, runtime.clone()).build();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let render_context = RenderContext::new(&editor, ingress.clone(), editor.redraw_handle());
        let entries = (0..24)
            .map(|index| test_item(&format!("pkg-{index:02}"), PkgKind::Lsp, None, "rust"))
            .collect();
        let updates = (0..24)
            .map(|index| {
                test_change(
                    &format!("pkg-{index:02}"),
                    PkgKind::Lsp,
                    Some("1"),
                    Some("2"),
                    None,
                )
            })
            .collect();
        let mut manager = PkgManager::new(
            PkgManagerData {
                entries,
                updates,
                registries: Vec::new(),
            },
            ingress,
        );
        manager.set_tab(PkgManagerTab::Updates);
        manager.move_last();
        manager.scroll = usize::MAX;
        manager.search_active = true;
        manager.search_query = "a long package search".to_owned();
        let cursor_before = manager.cursor(area, &editor);
        let layout = PkgManager::layout(area, manager.detail_zoom);
        let expected_scroll = clamp_list_scroll(
            layout.list,
            manager.rows.len(),
            Some(manager.selection),
            manager.scroll,
        );

        let _prepared = manager.prepare_render(area, &render_context);

        assert_eq!(manager.tab_area, layout.tabs);
        assert_eq!(manager.scroll, expected_scroll);
        assert!(manager.tab_scroll > 0);
        let active_range = manager
            .tab_state
            .tab_ranges
            .iter()
            .find(|range| range.index == PkgManagerTab::Updates.index_in(manager.profile.tabs()))
            .expect("active tab is visible");
        assert_eq!(
            manager.tab_state.tab_at(active_range.visible.start),
            Some(PkgManagerTab::Updates.index_in(manager.profile.tabs()))
        );
        assert_eq!(manager.cursor(area, &editor), cursor_before);
        assert!(matches!(cursor_before, (Some(_), CursorKind::Bar)));
    }

    #[test]
    fn equivalent_package_manager_snapshots_render_match() {
        let area = Rect::new(0, 0, 80, 24);
        let runtime = helix_runtime::test::runtime();
        let editor = helix_view::editor::EditorBuilder::new(area, runtime.clone()).build();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let render_context = RenderContext::new(&editor, ingress.clone(), editor.redraw_handle());
        let data = PkgManagerData {
            entries: (0..24)
                .map(|index| {
                    test_item(
                        &format!("pkg-{index:02}"),
                        PkgKind::Lsp,
                        (index % 2 == 0).then_some("1"),
                        "rust",
                    )
                })
                .collect(),
            updates: vec![test_change(
                "pkg-00",
                PkgKind::Lsp,
                Some("1"),
                Some("2"),
                None,
            )],
            registries: Vec::new(),
        };
        let mut direct_manager = PkgManager::new(data.clone(), ingress.clone());
        let mut deferred_manager = PkgManager::new(data, ingress);
        for manager in [&mut direct_manager, &mut deferred_manager] {
            manager.search_active = true;
            manager.search_query = "pkg".to_owned();
            manager.rebuild_filter();
            manager.move_last();
            manager.scroll = usize::MAX;
            Arc::make_mut(&mut manager.marked).insert("pkg-02".to_owned());
            manager.apply_progress_event(&OpEvent::Progress {
                name: "pkg-04".to_owned(),
                message: "download".to_owned(),
                percent: Some(42),
            });
        }
        let mut direct_surface =
            crate::render::CellSurface::empty(tui::ratatui::to_ratatui_rect(area));
        let mut deferred_surface =
            crate::render::CellSurface::empty(tui::ratatui::to_ratatui_rect(area));

        let prepared = direct_manager.prepare_render(area, &render_context);
        crate::render::CacheStore::default().compose(prepared, &mut direct_surface);
        let prepared = deferred_manager.prepare_render(area, &render_context);
        crate::render::CacheStore::default().compose(prepared, &mut deferred_surface);

        assert_eq!(direct_surface, deferred_surface);
        assert_eq!(direct_manager.scroll, deferred_manager.scroll);
        assert_eq!(direct_manager.tab_state, deferred_manager.tab_state);
        assert_eq!(direct_manager.tab_area, deferred_manager.tab_area);
    }

    #[test]
    fn cancelled_package_render_snapshot_does_not_paint() {
        use std::sync::atomic::AtomicU64;

        let area = Rect::new(0, 0, 40, 12);
        let runtime = helix_runtime::test::runtime();
        let editor = helix_view::editor::EditorBuilder::new(area, runtime.clone()).build();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let render_context = RenderContext::new(&editor, ingress.clone(), editor.redraw_handle());
        let mut manager = PkgManager::new(
            PkgManagerData {
                entries: vec![test_item("demo", PkgKind::Lsp, None, "rust")],
                updates: Vec::new(),
                registries: Vec::new(),
            },
            ingress,
        );
        let snapshot = manager.prepare_render_snapshot(area, &render_context);
        let cancellation =
            crate::render::RenderCancellation::for_sequence(Arc::new(AtomicU64::new(2)), 1);

        let output = snapshot.render(&cancellation);

        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                assert_eq!(output.surface()[(x, y)].symbol(), " ");
            }
        }
    }

    #[test]
    fn tab_model_uses_update_badge_count_from_plan() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
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

        assert_eq!(
            PkgManagerTab::from_index_in(2, manager.profile.tabs()),
            Some(PkgManagerTab::Updates)
        );
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
    fn acp_manager_defaults_to_acp_packages() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let manager = PkgManager::new_with_options(
            PkgManagerData {
                entries: vec![
                    test_item("rust-analyzer", PkgKind::Lsp, Some("1"), "rust"),
                    test_item("claude-agent", PkgKind::Acp, None, ""),
                ],
                updates: Vec::new(),
                registries: Vec::new(),
            },
            ingress,
            "ACP Agents",
            Some(PkgKind::Acp),
        );

        assert_eq!(manager.title, "ACP Agents");
        assert_eq!(manager.profile, PkgManagerProfile::AcpAgents);
        assert_eq!(manager.kind_filter, Some(PkgKind::Acp));
        assert_eq!(
            manager.profile.tabs(),
            &[
                PkgManagerTab::Browse,
                PkgManagerTab::Installed,
                PkgManagerTab::Updates
            ]
        );
        assert!(manager
            .rows
            .iter()
            .all(|row| matches!(row, PkgRow::Section(PkgKind::Acp) | PkgRow::Package(1))));
    }

    #[test]
    fn acp_manager_hides_package_only_actions() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let mut manager = PkgManager::new_with_options(
            PkgManagerData {
                entries: Vec::new(),
                updates: Vec::new(),
                registries: Vec::new(),
            },
            ingress,
            "ACP Agents",
            Some(PkgKind::Acp),
        );

        assert!(manager
            .binding_for_key(&KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::NONE,
            })
            .is_some());
        assert!(manager
            .binding_for_key(&KeyEvent {
                code: KeyCode::Char('f'),
                modifiers: KeyModifiers::NONE,
            })
            .is_none());
        assert!(manager
            .binding_for_key(&KeyEvent {
                code: KeyCode::Char('R'),
                modifiers: KeyModifiers::NONE,
            })
            .is_none());

        manager.set_tab_index(3);
        assert_eq!(manager.tab, PkgManagerTab::Browse);
    }

    #[test]
    fn row_state_maps_installed_available_working_problem() {
        let installed = test_item("installed", PkgKind::Lsp, Some("1"), "rust");
        let available = test_item("available", PkgKind::Lsp, None, "rust");
        let mut external = test_item("external", PkgKind::Lsp, None, "rust");
        external.provider = test_external_provider("external");
        let progress = PkgStatusline {
            label: "pkg available".to_owned(),
            percent: Some(42),
        };
        assert_eq!(row_state(&installed, None, None), RowState::Installed);
        assert_eq!(row_state(&available, None, None), RowState::Available);
        assert_eq!(row_state(&external, None, None), RowState::Installed);
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
    fn row_glyph_uses_inline_mark_without_hiding_progress_or_errors() {
        assert_eq!(row_glyph(RowState::Available, false), "○");
        assert_eq!(row_glyph(RowState::Available, true), "●");
        assert_eq!(row_glyph(RowState::Update, true), "●");
        assert_eq!(row_glyph(RowState::Installed, true), "●");
        assert_eq!(row_glyph(RowState::Working, true), "◍");
        assert_eq!(row_glyph(RowState::Problem, true), "⚠");
    }

    #[test]
    fn package_rows_label_installed_state_explicitly() {
        let installed = test_item("installed", PkgKind::Lsp, Some("1.2.3"), "rust");
        let available = test_item("available", PkgKind::Lsp, None, "rust");
        let mut external = test_item("external", PkgKind::Lsp, None, "rust");
        external.provider = test_external_provider("external");

        assert_eq!(status_label(&installed), "installed 1.2.3");
        assert_eq!(status_label(&external), "on PATH");
        assert_eq!(status_label(&available), "available");
    }

    #[test]
    fn config_only_rows_are_not_installable() {
        let mut configured = test_item("demo-ls", PkgKind::Lsp, None, "demo");
        configured.installable = false;

        assert!(!configured.is_installable());
        assert_eq!(status_label(&configured), "unpackaged");
    }

    #[test]
    fn configured_tools_include_default_health_commands() {
        let grammars = helix_loader::grammar::configured_grammar_names().unwrap();
        let tools = configured_tools(&default_lang_config(), &grammars);

        assert!(tools.iter().any(|tool| {
            tool.kind == PkgKind::Lsp
                && tool.name == "rust-analyzer"
                && tool.command == "rust-analyzer"
                && tool.languages.contains("rust")
        }));
        assert!(tools.iter().any(|tool| {
            tool.kind == PkgKind::Grammar && tool.name == "rust" && tool.languages.contains("rust")
        }));
        assert!(
            tools
                .iter()
                .filter(|tool| tool.kind == PkgKind::Grammar)
                .count()
                > 250
        );
    }

    #[test]
    fn installed_tab_includes_external_tools_without_pkg_receipts() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let available = test_item("available", PkgKind::Lsp, None, "rust");
        let mut external = test_item("external", PkgKind::Lsp, None, "rust");
        external.provider = test_external_provider("external");
        let mut manager = PkgManager::new(
            PkgManagerData {
                entries: vec![available, external],
                updates: Vec::new(),
                registries: Vec::new(),
            },
            ingress,
        );

        manager.set_tab(PkgManagerTab::Installed);

        assert!(manager.rows.contains(&PkgRow::Package(1)));
        assert!(!manager.rows.contains(&PkgRow::Package(0)));
    }

    #[test]
    fn broken_managed_package_stays_installed_and_can_be_repaired() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let mut broken = test_item("broken", PkgKind::Lsp, Some("1.2.3"), "rust");
        broken.provider = CapabilityProvider::BrokenManaged {
            package: "broken".to_owned(),
            version: "1.2.3".to_owned(),
            message: "managed command is missing".to_owned(),
        };
        let mut manager = PkgManager::new(
            PkgManagerData {
                entries: vec![broken],
                updates: Vec::new(),
                registries: Vec::new(),
            },
            ingress,
        );

        manager.set_tab(PkgManagerTab::Installed);

        assert!(manager.rows.contains(&PkgRow::Package(0)));
        assert!(manager.entries[0].is_pkg_managed());
        assert!(!manager.entries[0].is_usable());
        assert!(manager.entries[0].can_install());
        assert_eq!(status_label(&manager.entries[0]), "broken pkg 1.2.3");
    }

    #[test]
    fn installability_distinguishes_managed_external_and_broken_providers() {
        let managed = test_item("managed", PkgKind::Lsp, Some("1"), "rust");
        let mut external = test_item("external", PkgKind::Lsp, None, "rust");
        external.provider = test_external_provider("external");
        let mut broken = test_item("broken", PkgKind::Lsp, Some("1"), "rust");
        broken.provider = CapabilityProvider::BrokenManaged {
            package: "broken".to_owned(),
            version: "1".to_owned(),
            message: "missing".to_owned(),
        };

        assert!(!managed.can_install());
        assert!(external.can_install());
        assert!(broken.can_install());
    }

    #[test]
    fn tab_keys_switch_package_manager_tabs() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
        let manager = PkgManager::new(
            PkgManagerData {
                entries: Vec::new(),
                updates: Vec::new(),
                registries: Vec::new(),
            },
            ingress,
        );
        let next = manager
            .binding_for_key(&KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
            })
            .expect("tab binding");
        let next_vim = manager
            .binding_for_key(&KeyEvent {
                code: KeyCode::Char('L'),
                modifiers: KeyModifiers::NONE,
            })
            .expect("shift-l binding");
        let previous = manager
            .binding_for_key(&KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::SHIFT,
            })
            .expect("shift-tab binding");
        let previous_vim = manager
            .binding_for_key(&KeyEvent {
                code: KeyCode::Char('H'),
                modifiers: KeyModifiers::NONE,
            })
            .expect("shift-h binding");

        assert!(matches!(next.action, PkgAction::NextTab));
        assert!(matches!(next_vim.action, PkgAction::NextTab));
        assert!(matches!(previous.action, PkgAction::PreviousTab));
        assert!(matches!(previous_vim.action, PkgAction::PreviousTab));
    }

    #[test]
    fn remove_confirmation_label_reports_selected_installed_subset() {
        assert_eq!(
            remove_confirmation_label(&["amp".to_owned()], 0),
            "Remove package amp?"
        );
        assert_eq!(
            remove_confirmation_label(&["amp".to_owned(), "zed".to_owned()], 1),
            "Remove 2 selected packages? (1 selected package not pkg-managed)"
        );
    }

    #[test]
    fn binding_help_entries_have_dispatch_bindings() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.clone());
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
                    | PkgAction::Connect
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

    #[test]
    fn pkg_progress_finished_revision_changes_only_when_operation_finishes() {
        let mut state = PkgProgressState::default();
        assert_eq!(state.finished_revision, 0);

        state.apply_inner(&OpEvent::Started {
            name: "rust-analyzer".into(),
        });
        state.apply_inner(&OpEvent::Progress {
            name: "rust-analyzer".into(),
            message: "download".into(),
            percent: Some(42),
        });
        assert_eq!(state.finished_revision, 0);

        state.apply_inner(&OpEvent::Done {
            name: "rust-analyzer".into(),
        });
        assert_eq!(state.finished_revision, 1);

        state.apply_inner(&OpEvent::Failed {
            name: "rust-analyzer".into(),
            message: "network".into(),
        });
        assert_eq!(state.finished_revision, 2);
    }

    fn test_item(
        name: &str,
        kind: PkgKind,
        installed: Option<&str>,
        language: &str,
    ) -> PkgManagerItem {
        let installed = installed.map(str::to_owned);
        let provider = installed
            .as_ref()
            .map(|version| CapabilityProvider::Managed {
                version: version.clone(),
            })
            .unwrap_or(CapabilityProvider::Missing);
        let doctor = if installed.is_some() {
            "ok".to_owned()
        } else {
            "not installed".to_owned()
        };
        PkgManagerItem {
            name: name.to_owned(),
            kind,
            installed,
            provider,
            latest: "registry".to_owned(),
            description: format!("{name} package"),
            homepage: None,
            aliases: String::new(),
            categories: kind.default_category().to_owned(),
            languages: language.to_owned(),
            schemas: String::new(),
            source: "test".to_owned(),
            receipt: None,
            doctor,
            problem: None,
            installable: true,
            search_blob: format!("{name}\n{kind}\n{language}"),
        }
    }

    fn test_external_provider(command: &str) -> CapabilityProvider {
        CapabilityProvider::Command {
            command: command.to_owned(),
            path: format!("/usr/bin/{command}").into(),
            source: CapabilityProviderSource::Path,
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
