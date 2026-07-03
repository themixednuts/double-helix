use std::collections::{BTreeMap, HashMap};

use helix_core::unicode::width::UnicodeWidthStr;
use helix_pkg::{OpEvent, Ops, PackageSpec, PkgKind, Receipt, Source};
use helix_view::{
    graphics::{CursorKind, Rect, Style},
    input::{KeyEvent, MouseEventKind},
    keyboard::{KeyCode, KeyModifiers},
    Editor,
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

impl PkgProgressState {
    pub fn apply(&mut self, event: &OpEvent) {
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
}

const ID: &str = "pkg-manager";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PkgManagerTab {
    All,
    Installed,
    Lsp,
    Dap,
    Grammar,
    Plugin,
}

impl PkgManagerTab {
    const ALL: [Self; 6] = [
        Self::All,
        Self::Installed,
        Self::Lsp,
        Self::Dap,
        Self::Grammar,
        Self::Plugin,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Installed => "Installed",
            Self::Lsp => "LSP",
            Self::Dap => "DAP",
            Self::Grammar => "Grammar",
            Self::Plugin => "Plugins",
        }
    }

    const fn kind(self) -> Option<PkgKind> {
        match self {
            Self::Lsp => Some(PkgKind::Lsp),
            Self::Dap => Some(PkgKind::Dap),
            Self::Grammar => Some(PkgKind::Grammar),
            Self::Plugin => Some(PkgKind::Plugin),
            Self::All | Self::Installed => None,
        }
    }

    fn matches(self, item: &PkgManagerItem) -> bool {
        match self {
            Self::All => true,
            Self::Installed => item.installed.is_some(),
            Self::Lsp | Self::Dap | Self::Grammar | Self::Plugin => self.kind() == Some(item.kind),
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
    pub source: String,
    search_blob: String,
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
    hints: Rect,
}

pub struct PkgManager {
    entries: Vec<PkgManagerItem>,
    filtered: Vec<usize>,
    tab: PkgManagerTab,
    selection: usize,
    scroll: usize,
    tab_scroll: u16,
    search_query: String,
    search_active: bool,
    ingress: crate::runtime::RuntimeIngress,
}

pub fn manager(
    _editor: &helix_view::Editor,
    ingress: crate::runtime::RuntimeIngress,
) -> anyhow::Result<PkgManager> {
    let ops = Ops::open_default()?;
    let entries = load_entries(&ops)?;
    Ok(PkgManager::new(entries, ingress))
}

fn load_entries(ops: &Ops) -> anyhow::Result<Vec<PkgManagerItem>> {
    let receipts: HashMap<(PkgKind, String), Receipt> = ops
        .store()
        .receipts()?
        .into_iter()
        .map(|receipt| ((receipt.kind, receipt.name.clone()), receipt))
        .collect();

    let mut entries: Vec<_> = ops
        .registry()
        .iter()
        .map(|package| item_from_package(package, &receipts))
        .collect();
    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

fn item_from_package(
    package: &PackageSpec,
    receipts: &HashMap<(PkgKind, String), Receipt>,
) -> PkgManagerItem {
    let installed = receipts
        .get(&(package.kind, package.name.clone()))
        .map(|receipt| receipt.version.clone());
    let latest = package
        .version
        .tag_source
        .as_deref()
        .unwrap_or("registry")
        .to_owned();
    let aliases = package.aliases.join(", ");
    let categories = package.categories.join(", ");
    let languages = package.languages.join(", ");
    let source = source_label(package);
    let search_blob = package.search_terms().collect::<Vec<_>>().join("\n");
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
        source,
        search_blob,
    }
}

fn source_label(package: &PackageSpec) -> String {
    let mut labels = Vec::new();
    for artifact in &package.artifacts {
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
    if source.github_release.is_some() {
        Some("github".to_owned())
    } else if source.archive.is_some() {
        Some("archive".to_owned())
    } else if source.npm.is_some() {
        Some("npm".to_owned())
    } else if source.pip.is_some() {
        Some("pip".to_owned())
    } else if source.cargo.is_some() {
        Some("cargo".to_owned())
    } else if source.go.is_some() {
        Some("go".to_owned())
    } else if source.git.is_some() {
        Some("git".to_owned())
    } else if source.system.is_some() {
        Some("system".to_owned())
    } else if source.native.is_some() {
        Some("native".to_owned())
    } else if source.plugin.is_some() || source.plugin_ref.is_some() {
        Some("plugin".to_owned())
    } else {
        None
    }
}

impl PkgManager {
    fn new(entries: Vec<PkgManagerItem>, ingress: crate::runtime::RuntimeIngress) -> Self {
        let mut manager = Self {
            entries,
            filtered: Vec::new(),
            tab: PkgManagerTab::All,
            selection: 0,
            scroll: 0,
            tab_scroll: 0,
            search_query: String::new(),
            search_active: false,
            ingress,
        };
        manager.rebuild_filter();
        manager
    }

    fn selected_item(&self) -> Option<&PkgManagerItem> {
        self.filtered
            .get(self.selection)
            .and_then(|index| self.entries.get(*index))
    }

    fn rebuild_filter(&mut self) {
        let selected_name = self.selected_item().map(|item| item.name.clone());
        let query = self.search_query.trim().to_ascii_lowercase();
        self.filtered = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, item)| self.tab.matches(item))
            .filter(|(_, item)| {
                query.is_empty() || item.search_blob.to_ascii_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect();

        if self.filtered.is_empty() {
            self.selection = 0;
            self.scroll = 0;
            return;
        }

        self.selection = selected_name
            .and_then(|name| {
                self.filtered
                    .iter()
                    .position(|index| self.entries[*index].name == name)
            })
            .unwrap_or_else(|| self.selection.min(self.filtered.len() - 1));
        self.ensure_selection_visible();
    }

    fn set_tab(&mut self, tab: PkgManagerTab) {
        self.tab = tab;
        self.selection = 0;
        self.scroll = 0;
        self.rebuild_filter();
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let max = self.filtered.len().saturating_sub(1) as isize;
        self.selection = (self.selection as isize + delta).clamp(0, max) as usize;
        self.ensure_selection_visible();
    }

    fn ensure_selection_visible(&mut self) {
        if self.selection < self.scroll {
            self.scroll = self.selection;
        }
    }

    fn layout(area: Rect) -> PkgManagerLayout {
        use helix_view::layout::{split_horizontal, split_vertical, Size};

        let inner = crate::widgets::Panel::framed(crate::widgets::PanelStyle::default(), false)
            .content_area(area)
            .inner(helix_view::graphics::Margin::horizontal(1));
        let rows = split_vertical(
            inner,
            &[Size::fixed(1), Size::fixed(1), Size::Fill, Size::fixed(1)],
        );
        let tabs = rows[0];
        let search = rows[1];
        let hints = rows[3];
        let content = rows[2].clip_top(1);
        let (list, detail) = if content.width >= 86 {
            let cols = split_horizontal(content, &[Size::Percent(42), Size::Fill]);
            (cols[0].clip_right(1), cols[1].clip_left(1))
        } else {
            let panes = split_vertical(content, &[Size::Percent(45), Size::Fill]);
            (panes[0].clip_bottom(1), panes[1].clip_top(1))
        };
        let search_input = Rect::new(
            search.x.saturating_add(3),
            search.y,
            search.width.saturating_sub(4),
            1,
        );

        PkgManagerLayout {
            tabs,
            search,
            search_input,
            list,
            detail,
            hints,
        }
    }

    fn start_operation(&self, cx: &mut Context, operation: crate::runtime::PkgOperation) {
        crate::runtime::spawn_pkg_operation(operation, cx.editor.work(), self.ingress.clone());
    }

    fn install_selected(&self, cx: &mut Context) {
        let Some(item) = self.selected_item() else {
            return;
        };
        self.start_operation(
            cx,
            crate::runtime::PkgOperation::Install(vec![item.name.clone()]),
        );
    }

    fn update_selected(&self, cx: &mut Context) {
        let Some(item) = self.selected_item() else {
            return;
        };
        self.start_operation(
            cx,
            crate::runtime::PkgOperation::Update(vec![item.name.clone()]),
        );
    }

    fn remove_selected(&self, cx: &mut Context) -> EventResult {
        let Some(item) = self.selected_item() else {
            return EventResult::Consumed(None);
        };
        if item.installed.is_none() {
            cx.editor
                .set_status(format!("{} is not installed", item.name));
            return EventResult::Consumed(None);
        }

        let name = item.name.clone();
        let ingress = self.ingress.clone();
        let confirmation = crate::ui::picker::PickerConfirmation::new(
            format!("Remove package {name}?"),
            move |cx| {
                crate::runtime::spawn_pkg_operation(
                    crate::runtime::PkgOperation::Remove(vec![name.clone()]),
                    cx.editor.work(),
                    ingress.clone(),
                );
            },
        );
        EventResult::Consumed(Some(confirmation.into_post_action()))
    }

    fn reload(&mut self, cx: &mut Context) {
        let result: anyhow::Result<Vec<PkgManagerItem>> = (|| {
            let ops = Ops::open_default()?;
            load_entries(&ops)
        })();
        match result {
            Ok(entries) => {
                self.entries = entries;
                self.rebuild_filter();
                cx.editor.set_status("Package registry refreshed");
            }
            Err(err) => cx
                .editor
                .set_error(format!("Failed to refresh package registry: {err}")),
        }
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

        if matches!(
            key,
            KeyEvent {
                code: KeyCode::Char('/'),
                modifiers: KeyModifiers::NONE,
            }
        ) {
            self.search_active = true;
            return true;
        }

        false
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
        let layout = Self::layout(area);
        let panel_style =
            crate::widgets::PanelStyle::new(styles.background, styles.border, styles.title);
        crate::widgets::Panel::framed(panel_style, cx.config().rounded_corners)
            .title(" Package Manager ")
            .render(surface, area);

        self.render_tabs(surface, layout.tabs, styles);
        self.render_search(surface, layout.search, layout.search_input, styles);
        self.render_list(surface, layout.list, styles);
        self.render_detail(surface, layout.detail, styles);
        self.render_hints(surface, layout.hints, styles);
    }

    fn render_tabs(
        &mut self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        styles: PkgManagerStyles,
    ) {
        let tabs = PkgManagerTab::ALL
            .iter()
            .copied()
            .map(|tab| {
                let count = self.entries.iter().filter(|item| tab.matches(item)).count();
                crate::widgets::Tab::new(tab.label()).badge(count.to_string())
            })
            .collect::<Vec<_>>();
        let active = PkgManagerTab::ALL
            .iter()
            .position(|tab| *tab == self.tab)
            .unwrap_or(0);
        let state = crate::widgets::tabs_with_options(
            surface,
            area,
            &tabs,
            crate::widgets::TabsOptions::new(active)
                .scroll(self.tab_scroll)
                .separator(" "),
            crate::widgets::TabsStyle {
                background: styles.background,
                active: styles.title,
                inactive: styles.inactive,
                badge: styles.inactive,
                separator: styles.background,
                overflow: styles.inactive,
                hover: styles.selection,
            },
        );
        self.tab_scroll = state.scroll;
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
        if self.search_query.is_empty() && !self.search_active {
            surface.set_stringn(
                input_area.x,
                input_area.y,
                "search packages",
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
        let state = crate::widgets::item_list(
            surface,
            area,
            self.filtered.len(),
            (!self.filtered.is_empty()).then_some(self.selection),
            self.scroll,
            &list_styles,
            |visible_index, row_area, surface, selected| {
                let item = &self.entries[self.filtered[visible_index]];
                render_package_row(surface, row_area, item, selected, styles);
            },
        );
        self.scroll = state.scroll;
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
        let Some(item) = self.selected_item() else {
            surface.set_stringn(
                area.x,
                area.y,
                "No packages",
                area.width as usize,
                tui::ratatui::to_ratatui_style(styles.inactive),
            );
            return;
        };

        let mut y = area.y;
        write_line(surface, area, &mut y, &item.name, styles.title);
        write_line(
            surface,
            area,
            &mut y,
            &format!("{}  {}", item.kind, status_label(item)),
            if item.installed.is_some() {
                styles.installed
            } else {
                styles.available
            },
        );
        write_blank(&mut y, area);
        write_wrapped(surface, area, &mut y, &item.description, styles.text);
        write_blank(&mut y, area);
        write_field(
            surface,
            area,
            &mut y,
            "Installed",
            item.installed.as_deref().unwrap_or("-"),
            styles,
        );
        write_field(surface, area, &mut y, "Latest", &item.latest, styles);
        write_field(surface, area, &mut y, "Source", &item.source, styles);
        write_field(
            surface,
            area,
            &mut y,
            "Languages",
            empty_dash(&item.languages),
            styles,
        );
        write_field(
            surface,
            area,
            &mut y,
            "Categories",
            empty_dash(&item.categories),
            styles,
        );
        write_field(
            surface,
            area,
            &mut y,
            "Aliases",
            empty_dash(&item.aliases),
            styles,
        );
        if let Some(homepage) = &item.homepage {
            write_field(surface, area, &mut y, "Homepage", homepage, styles);
        }
    }

    fn render_hints(
        &self,
        surface: &mut crate::render::CellSurface,
        area: Rect,
        styles: PkgManagerStyles,
    ) {
        let hints = [
            crate::widgets::Hint::new("Enter", "install").priority(220),
            crate::widgets::Hint::new("/", "search").priority(215),
            crate::widgets::Hint::new("Tab", "next tab").priority(210),
            crate::widgets::Hint::new("u", "update").priority(205),
            crate::widgets::Hint::new("d", "remove").priority(204),
            crate::widgets::Hint::new("r", "refresh").priority(203),
            crate::widgets::Hint::new("!", "doctor").priority(202),
            crate::widgets::Hint::new("Esc", "close").priority(201),
        ];
        crate::widgets::hint_bar(
            surface,
            area,
            &hints,
            crate::widgets::HintBarStyle {
                background: styles.title,
                key: styles.text,
                label: styles.inactive,
                separator: styles.inactive,
            },
        );
    }
}

fn render_package_row(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    item: &PkgManagerItem,
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
    let text_style = if selected { styles.text } else { styles.text };
    let status_style = if item.installed.is_some() {
        styles.installed
    } else {
        styles.available
    };
    surface.set_style(
        tui::ratatui::to_ratatui_rect(area),
        tui::ratatui::to_ratatui_style(base),
    );

    let status = if item.installed.is_some() { "*" } else { " " };
    surface.set_stringn(
        area.x,
        area.y,
        status,
        1,
        tui::ratatui::to_ratatui_style(status_style),
    );
    let kind = item.kind.to_string();
    let kind_width = kind.width() as u16;
    if area.width > kind_width.saturating_add(2) {
        surface.set_stringn(
            area.right().saturating_sub(kind_width),
            area.y,
            &kind,
            kind_width as usize,
            tui::ratatui::to_ratatui_style(styles.inactive),
        );
    }
    let name_area = area
        .clip_left(2)
        .clip_right(kind_width.saturating_add(2).min(area.width));
    surface.set_stringn(
        name_area.x,
        name_area.y,
        &item.name,
        name_area.width as usize,
        tui::ratatui::to_ratatui_style(text_style),
    );
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
    surface.set_stringn(
        area.x,
        *y,
        text,
        area.width as usize,
        tui::ratatui::to_ratatui_style(style),
    );
    *y = y.saturating_add(1);
}

fn write_blank(y: &mut u16, area: Rect) {
    if *y < area.bottom() {
        *y = y.saturating_add(1);
    }
}

fn write_field(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    y: &mut u16,
    label: &str,
    value: &str,
    styles: PkgManagerStyles,
) {
    if *y >= area.bottom() {
        return;
    }
    let label_width = 11u16.min(area.width);
    surface.set_stringn(
        area.x,
        *y,
        label,
        label_width as usize,
        tui::ratatui::to_ratatui_style(styles.inactive),
    );
    if area.width > label_width {
        surface.set_stringn(
            area.x.saturating_add(label_width),
            *y,
            value,
            area.width.saturating_sub(label_width) as usize,
            tui::ratatui::to_ratatui_style(styles.text),
        );
    }
    *y = y.saturating_add(1);
}

fn write_wrapped(
    surface: &mut crate::render::CellSurface,
    area: Rect,
    y: &mut u16,
    text: &str,
    style: Style,
) {
    if area.width == 0 {
        return;
    }
    if text.is_empty() {
        write_line(surface, area, y, "No description", style);
        return;
    }
    let mut line = String::new();
    for word in text.split_whitespace() {
        let next_width = line
            .width()
            .saturating_add(if line.is_empty() { 0 } else { 1 })
            .saturating_add(word.width());
        if next_width > area.width as usize && !line.is_empty() {
            write_line(surface, area, y, &line, style);
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        write_line(surface, area, y, &line, style);
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
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let delta = match mouse.kind {
                        MouseEventKind::ScrollUp => -3,
                        MouseEventKind::ScrollDown => 3,
                        _ => 0,
                    };
                    self.move_selection(delta);
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

        match key {
            KeyEvent {
                code: KeyCode::Esc,
                modifiers: KeyModifiers::NONE,
            } => {
                if !self.search_query.is_empty() {
                    self.search_query.clear();
                    self.rebuild_filter();
                    EventResult::Consumed(None)
                } else {
                    EventResult::Consumed(Some(PostAction::PopLayer {
                        model_layer: None,
                        remember_picker: false,
                    }))
                }
            }
            KeyEvent {
                code: KeyCode::Down | KeyCode::Char('j'),
                modifiers: KeyModifiers::NONE,
            } => {
                self.move_selection(1);
                EventResult::Consumed(None)
            }
            KeyEvent {
                code: KeyCode::Up | KeyCode::Char('k'),
                modifiers: KeyModifiers::NONE,
            } => {
                self.move_selection(-1);
                EventResult::Consumed(None)
            }
            KeyEvent {
                code: KeyCode::Tab | KeyCode::Right,
                modifiers: KeyModifiers::NONE,
            } => {
                let current = PkgManagerTab::ALL
                    .iter()
                    .position(|tab| *tab == self.tab)
                    .unwrap_or(0);
                let next = (current + 1) % PkgManagerTab::ALL.len();
                self.set_tab(PkgManagerTab::ALL[next]);
                EventResult::Consumed(None)
            }
            KeyEvent {
                code: KeyCode::Tab | KeyCode::Left,
                modifiers: KeyModifiers::SHIFT,
            }
            | KeyEvent {
                code: KeyCode::Left,
                modifiers: KeyModifiers::NONE,
            } => {
                let current = PkgManagerTab::ALL
                    .iter()
                    .position(|tab| *tab == self.tab)
                    .unwrap_or(0);
                let next = current
                    .checked_sub(1)
                    .unwrap_or_else(|| PkgManagerTab::ALL.len() - 1);
                self.set_tab(PkgManagerTab::ALL[next]);
                EventResult::Consumed(None)
            }
            KeyEvent {
                code: KeyCode::Enter | KeyCode::Char('i'),
                modifiers: KeyModifiers::NONE,
            } => {
                self.install_selected(cx);
                EventResult::Consumed(None)
            }
            KeyEvent {
                code: KeyCode::Char('u'),
                modifiers: KeyModifiers::NONE,
            } => {
                self.update_selected(cx);
                EventResult::Consumed(None)
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::NONE,
            } => self.remove_selected(cx),
            KeyEvent {
                code: KeyCode::Char('!'),
                modifiers: KeyModifiers::NONE,
            } => {
                self.start_operation(cx, crate::runtime::PkgOperation::Doctor);
                EventResult::Consumed(None)
            }
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
            } => {
                self.reload(cx);
                EventResult::Consumed(None)
            }
            _ => EventResult::Consumed(None),
        }
    }

    fn cursor(&self, area: Rect, _editor: &Editor) -> (Option<helix_core::Position>, CursorKind) {
        if !self.search_active {
            return (None, CursorKind::Hidden);
        }
        let layout = Self::layout(area);
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
    fn pkg_manager_filters_by_tab_and_search() {
        let runtime = helix_runtime::test::runtime();
        let (ingress, _rx) = crate::runtime::RuntimeIngress::channel(runtime.work().clone());
        let mut manager = PkgManager::new(
            vec![
                test_item("rust-analyzer", PkgKind::Lsp, Some("2026-01-01"), "rust"),
                test_item("debugpy", PkgKind::Dap, None, "python"),
                test_item("tree-sitter-rust", PkgKind::Grammar, None, "rust"),
            ],
            ingress,
        );

        manager.set_tab(PkgManagerTab::Installed);
        assert_eq!(manager.filtered.len(), 1);
        assert_eq!(manager.selected_item().unwrap().name, "rust-analyzer");

        manager.set_tab(PkgManagerTab::All);
        manager.search_query = "python".to_owned();
        manager.rebuild_filter();
        assert_eq!(manager.filtered.len(), 1);
        assert_eq!(manager.selected_item().unwrap().name, "debugpy");
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
            source: "test".to_owned(),
            search_blob: format!("{name}\n{kind}\n{language}"),
        }
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
            message: "download 42%".into(),
            percent: Some(42),
        });
        assert_eq!(state.statusline().unwrap().percent, Some(42));

        state.apply(&OpEvent::Done {
            name: "rust-analyzer".into(),
        });
        assert_eq!(state.statusline(), None);
    }
}
