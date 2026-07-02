use super::*;
use std::any::Any;
use tui::ratatui::widgets::{Clear, Widget};

/// Horizontal gutter applied to header, nav, content, args, and footer. One
/// constant keeps the entire chrome breathing at the same rhythm.
const GUTTER: u16 = 2;

/// Width of the story navigator on the left.
const NAV_WIDTH: u16 = 34;

/// Maximum number of rows the args panel ever occupies at the bottom of the
/// content area (header row + arg rows).
const ARGS_MAX_HEIGHT: u16 = 6;

pub(super) fn render_storybook(
    surface: &mut Buffer,
    width: u16,
    height: u16,
    selected_id: &str,
    theme: &LoadedTheme,
) {
    render_storybook_with_state(surface, width, height, selected_id, theme, None, false);
}

pub(super) fn render_storybook_with_state(
    surface: &mut Buffer,
    width: u16,
    height: u16,
    selected_id: &str,
    theme: &LoadedTheme,
    interactive_state: Option<&dyn Any>,
    story_focused: bool,
) {
    let styles = theme.styles;
    let area = Rect::new(0, 0, width, height);
    let root = tui::ratatui::to_ratatui_rect(area);
    surface.set_style(root, styles.rat(styles.surface));
    Clear.render(root, surface);
    surface.set_style(root, styles.rat(styles.surface));

    let selected_index = STORIES
        .iter()
        .position(|story| story.id == selected_id)
        .unwrap_or(0);
    let story = STORIES[selected_index];

    let [header, body, footer] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(root);

    render_storybook_header(
        surface,
        tui::ratatui::to_helix_rect(header),
        selected_index,
        story,
        styles,
        &theme.name,
    );

    let [nav, content] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(NAV_WIDTH), Constraint::Min(40)])
        .areas(body);

    render_nav(
        surface,
        tui::ratatui::to_helix_rect(nav),
        selected_id,
        styles,
    );
    render_story_panel(
        surface,
        tui::ratatui::to_helix_rect(content),
        story,
        theme,
        interactive_state,
    );
    render_storybook_footer(
        surface,
        tui::ratatui::to_helix_rect(footer),
        selected_index,
        styles,
        story_focused,
    );
}

fn render_storybook_header(
    surface: &mut Buffer,
    area: Rect,
    selected_index: usize,
    story: Story,
    styles: UiStyleGuide,
    theme_name: &str,
) {
    fill_rect(surface, area, styles.surface);
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Row 0: brand mark + theme/counter
    // The diamond is the one signature accent in the entire shell — it appears
    // here and nowhere else. Treat it as the brand mark.
    let brand_x = area.x.saturating_add(GUTTER);
    if area.width > GUTTER {
        surface.set_stringn(brand_x, area.y, "◆", 1, styles.rat(styles.accent));
        if area.width > GUTTER + 2 {
            surface.set_stringn(
                brand_x.saturating_add(2),
                area.y,
                "Storybook",
                (area.width.saturating_sub(GUTTER + 2)) as usize,
                styles.rat(styles.text),
            );
        }
    }
    let counter = format!(
        "theme · {theme_name}    {:02} / {:02}",
        selected_index + 1,
        STORIES.len()
    );
    set_right_clipped(surface, gutter_row(area, 0), &counter, styles.muted);

    // Row 1: component · variant      story_id
    if area.height > 1 {
        let crumb = format!("{} · {}", story.component, story.variant);
        let crumb_row = padded_row(area, 1);
        set_clipped(surface, crumb_row, 0, &crumb, styles.text);
        set_right_clipped(surface, gutter_row(area, 1), story.id, styles.muted);
    }

    // Row 2: summary (one-line description)
    if area.height > 2 {
        set_clipped(surface, padded_row(area, 2), 0, story.summary, styles.muted);
    }
}

fn render_nav(surface: &mut Buffer, area: Rect, selected_id: &str, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    if area.width == 0 || area.height == 0 {
        return;
    }

    let selected_index = STORIES
        .iter()
        .position(|story| story.id == selected_id)
        .unwrap_or(0);

    let mut entries = Vec::new();
    let mut previous_component = "";
    for (index, story) in STORIES.iter().enumerate() {
        if story.component != previous_component {
            entries.push(NavEntry::Component(story.component));
            previous_component = story.component;
        }
        entries.push(NavEntry::Story(index));
    }

    let selected_entry = entries
        .iter()
        .position(|entry| matches!(entry, NavEntry::Story(index) if *index == selected_index))
        .unwrap_or(0);

    // One row of section header, one row of breathing space, then the list.
    let header_rows = 2u16.min(area.height);
    let list_height = area.height.saturating_sub(header_rows);
    let visible_entries = list_height.max(1) as usize;
    let mut first_entry = selected_entry.saturating_sub(visible_entries / 2);
    first_entry = first_entry.min(entries.len().saturating_sub(visible_entries));
    let last_entry = (first_entry + visible_entries).min(entries.len());

    let inner_width = area.width.saturating_sub(1); // reserve right column for divider/scrollbar
    let label_width = inner_width.saturating_sub(GUTTER);

    if header_rows > 0 && label_width > 0 {
        let header_row = Rect::new(area.x.saturating_add(GUTTER), area.y, label_width, 1);
        set_clipped(surface, header_row, 0, "STORIES", styles.muted);
    }

    let list_area = Rect::new(
        area.x,
        area.y.saturating_add(header_rows),
        inner_width,
        list_height,
    );

    for (row, entry) in entries[first_entry..last_entry].iter().enumerate() {
        let y = list_area.y.saturating_add(row as u16);
        if y >= list_area.bottom() {
            break;
        }
        let row_area = Rect::new(list_area.x, y, list_area.width, 1);
        match *entry {
            NavEntry::Component(component) => {
                surface.set_style(
                    tui::ratatui::to_ratatui_rect(row_area),
                    styles.rat(styles.surface),
                );
                // Component group label sits at the gutter, muted, unadorned.
                let label_area = Rect::new(
                    row_area.x.saturating_add(GUTTER),
                    row_area.y,
                    label_width,
                    1,
                );
                set_clipped(surface, label_area, 0, component, styles.muted);
            }
            NavEntry::Story(index) => {
                let story = STORIES[index];
                let selected = story.id == selected_id;
                let row_style = if selected {
                    styles.menu_selected
                } else {
                    styles.surface
                };
                // Filled background is the only selection cue — no arrow, no
                // bold prefix. Variants nest under their component header
                // with a half-step indent for hierarchy.
                surface.set_style(
                    tui::ratatui::to_ratatui_rect(row_area),
                    styles.rat(row_style),
                );
                let label_area = Rect::new(
                    row_area.x.saturating_add(GUTTER + 2),
                    row_area.y,
                    label_width.saturating_sub(2),
                    1,
                );
                set_clipped(surface, label_area, 0, story.variant, row_style);
            }
        }
    }

    if area.width > 0 {
        crate::widgets::vdivider(
            surface,
            Rect::new(area.right().saturating_sub(1), area.y, 1, area.height),
            styles.statusline_separator,
        );
    }

    if entries.len() > visible_entries && list_area.height > 0 && area.width > 1 {
        crate::widgets::Scrollbar::new(entries.len(), first_entry, visible_entries)
            .symbol("▌")
            .thumb_style(styles.menu_scroll)
            .render(
                Rect::new(
                    area.right().saturating_sub(1),
                    list_area.y,
                    1,
                    list_area.height,
                ),
                surface,
            );
    }
}

#[derive(Clone, Copy)]
enum NavEntry {
    Component(&'static str),
    Story(usize),
}

fn render_story_panel(
    surface: &mut Buffer,
    area: Rect,
    story: Story,
    theme: &LoadedTheme,
    interactive_state: Option<&dyn Any>,
) {
    let styles = theme.styles;
    fill_rect(surface, area, styles.surface);
    if area.width == 0 || area.height == 0 {
        return;
    }

    // The args panel is hidden when there isn't room for a meaningful canvas.
    let args_visible = !story.args.is_empty() && area.height >= 10;
    let args_height = if args_visible {
        ARGS_MAX_HEIGHT.min(area.height)
    } else {
        0
    };
    let canvas_height = area.height.saturating_sub(args_height);

    // Padded canvas — the story sees an area inset from the divider so its
    // content has breathing room and never butts against the nav rule.
    let canvas = Rect::new(
        area.x.saturating_add(GUTTER),
        area.y,
        area.width.saturating_sub(GUTTER * 2).max(1),
        canvas_height,
    );
    if let Some(state) = interactive_state {
        story.render_with_state(surface, canvas, theme.context(), Some(state));
    } else {
        story.render(surface, canvas, theme.context());
    }

    if args_visible {
        let args = Rect::new(
            area.x.saturating_add(GUTTER),
            area.y.saturating_add(canvas_height),
            area.width.saturating_sub(GUTTER * 2).max(1),
            args_height,
        );
        render_story_args_panel(surface, args, story, styles);
    }
}

fn render_story_args_panel(surface: &mut Buffer, area: Rect, story: Story, styles: UiStyleGuide) {
    fill_rect(surface, area, styles.surface);
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Header row: muted section label on the left, story tags on the right.
    // No horizontal rule — the empty row above is the separator.
    let meta = format!(
        "{} · {} · {}",
        story.variant,
        story.kind.label(),
        story.canvas.label()
    );
    set_clipped(surface, area, 0, "PROPS", styles.muted);
    set_right_clipped(surface, area.with_height(1), &meta, styles.muted);

    // Three-column rows: name (accent) · value (text) · description (muted).
    let rows = area.height.saturating_sub(1) as usize;
    for (idx, arg) in story.args.iter().take(rows).enumerate() {
        let y = area.y.saturating_add(1 + idx as u16);
        let row = Rect::new(area.x, y, area.width, 1);
        surface.set_stringn(row.x, row.y, arg.name, 16, styles.rat(styles.accent));
        surface.set_stringn(
            row.x.saturating_add(18),
            row.y,
            arg.value,
            28,
            styles.rat(styles.text),
        );
        set_clipped(
            surface,
            Rect::new(
                row.x.saturating_add(48),
                row.y,
                row.width.saturating_sub(48),
                1,
            ),
            0,
            arg.description,
            styles.muted,
        );
    }
}

fn render_storybook_footer(
    surface: &mut Buffer,
    area: Rect,
    selected_index: usize,
    styles: UiStyleGuide,
    story_focused: bool,
) {
    fill_rect(surface, area, styles.surface);
    if area.width == 0 || area.height == 0 {
        return;
    }

    let counter = format!("{:02} / {:02}", selected_index + 1, STORIES.len());
    let left_row = Rect::new(area.x.saturating_add(GUTTER), area.y, area.width, 1);
    set_clipped(surface, left_row, 0, &counter, styles.muted);

    // Three actions, generous spacing — reads as a single keyboard hint row,
    // not a wall of CLI flags.
    let right = if story_focused {
        "story keys active    q quit"
    } else {
        "↑↓ story    ⇥ theme    q quit"
    };
    set_right_clipped(surface, gutter_row(area, 0), right, styles.muted);
}

/// One-line rect at row `row` within `area`, inset by GUTTER on the left.
fn padded_row(area: Rect, row: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(GUTTER),
        area.y.saturating_add(row),
        area.width.saturating_sub(GUTTER),
        1,
    )
}

/// One-line rect at row `row` within `area`, inset by GUTTER on the right
/// (so `set_right_clipped` leaves the same gutter as the left side).
fn gutter_row(area: Rect, row: u16) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(row),
        area.width.saturating_sub(GUTTER),
        1,
    )
}
