use crate::compositor::{Component, Context, Event, EventResult, RenderContext};
use crate::runtime::RuntimeTaskEvent;
use crate::ui::gradient_border::GradientBorder;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::theme::Modifier;
use helix_view::{
    editor::{
        CmdlineStyle, Config, Notification, NotificationConfig, NotificationEmojis,
        NotificationIcons, NotificationPosition, NotificationShadowConfig, NotificationStyle,
        Severity,
    },
    graphics::{Color, Rect, Style},
    Theme,
};
use std::hash::{Hash, Hasher};
use std::time::Instant;
use tokio::time::sleep as tokio_sleep;

// ---------------------------------------------------------------------------
// Stateful popup — tracks notification lifecycle (add / expire / dismiss)
// ---------------------------------------------------------------------------

pub struct NotificationPopup {
    notifications: Vec<NotificationItem>,
    gradient_border: Option<GradientBorder>,
    layout_thickness: u16,
    layout_rounded: bool,
    layout_padding: u16,
}

#[derive(Debug, Clone)]
struct NotificationItem {
    notification: Notification,
    area: Rect,
    fade_start: Option<Instant>,
}

impl NotificationItem {
    fn new(notification: Notification) -> Self {
        Self {
            notification,
            area: Rect::default(),
            fade_start: None,
        }
    }

    fn fade_progress(&self) -> f32 {
        if let Some(start) = self.fade_start {
            let elapsed = start.elapsed().as_millis() as f32;
            (elapsed / 300.0).min(1.0)
        } else {
            0.0
        }
    }
}

impl NotificationPopup {
    pub fn new() -> Self {
        Self {
            notifications: Vec::new(),
            gradient_border: None,
            layout_thickness: 1,
            layout_rounded: false,
            layout_padding: 1,
        }
    }

    pub fn update(
        &mut self,
        config: &Config,
        notifications: &[Notification],
        work: helix_runtime::Work,
        ingress: crate::runtime::RuntimeIngress,
    ) {
        let notifications_config = &config.notifications;

        if !notifications_config.enable
            || notifications_config.style != NotificationStyle::Popup
            || config.cmdline.style != CmdlineStyle::Popup
        {
            self.notifications.clear();
            return;
        }

        let is_active =
            |notification: &Notification| !notification.dismissed && !notification.is_expired();

        self.notifications.retain_mut(|item| {
            let still_active = notifications
                .iter()
                .any(|n| n.id == item.notification.id && is_active(n));
            let is_expired = item.notification.is_expired();
            still_active && !is_expired
        });

        for notification in notifications
            .iter()
            .filter(|notification| is_active(notification))
        {
            if !self
                .notifications
                .iter()
                .any(|item| item.notification.id == notification.id)
            {
                let id = notification.id;
                let timeout_opt = notification.timeout;
                self.notifications
                    .push(NotificationItem::new(notification.clone()));

                if let Some(timeout) = timeout_opt {
                    let started = notification.timestamp;
                    let elapsed = started.elapsed();
                    let remaining = if timeout > elapsed {
                        timeout - elapsed
                    } else {
                        std::time::Duration::from_millis(0)
                    };
                    let ingress = ingress.clone();

                    work.spawn(async move {
                        tokio_sleep(remaining).await;
                        crate::runtime::send_task_event_with(
                            RuntimeTaskEvent::DismissNotification { id },
                            ingress,
                        )
                        .await;
                    })
                    .detach();
                }
            }
        }
    }

    /// Sync state from editor and produce a native Ratatui render snapshot.
    /// Returns `None` when there are no notifications to draw.
    pub fn prepare_snapshot(
        &mut self,
        area: Rect,
        cx: &RenderContext,
    ) -> Option<crate::render::PreparedRender> {
        let config = cx.config();
        let config = &*config;
        self.update(
            config,
            cx.notification_history(),
            cx.work(),
            cx.ingress.clone(),
        );
        let notifications_config = &config.notifications;

        self.layout_thickness = if config.gradient_borders.enable {
            config.gradient_borders.thickness as u16
        } else {
            notifications_config.border.width as u16
        };
        self.layout_rounded = config.rounded_corners || notifications_config.border.radius > 0;
        self.layout_padding = notifications_config.padding;

        if self.notifications.is_empty() {
            return None;
        }

        self.calculate_notification_areas(area, notifications_config);

        let model = NotificationModel::collect(self, config, cx.theme());
        Some(prepare_notification_render(model, area))
    }

    fn calculate_notification_areas(&mut self, viewport: Rect, config: &NotificationConfig) {
        let max_width = config.max_width.min(viewport.width.saturating_sub(4));
        let spacing = 1u16;
        let mut y_offset = 0u16;

        let mut areas = Vec::new();
        for item in &self.notifications {
            let message = &item.notification.message;
            let wrap_inner_width = max_width
                .saturating_sub(self.layout_thickness * 2)
                .saturating_sub(self.layout_padding * 2)
                .max(1);

            let mut prefix = String::new();
            if config.show_emojis {
                prefix.push_str(notification_emoji(
                    &item.notification.severity,
                    &config.emojis,
                ));
                prefix.push(' ');
            } else if config.show_icons {
                prefix.push_str(notification_icon(
                    &item.notification.severity,
                    &config.icons,
                ));
                prefix.push(' ');
            }
            let prefix_width = prefix.width() as u16;

            let content_lines = wrap_text(message, wrap_inner_width);
            let content_height = content_lines.len() as u16;

            let mut content_max_w: u16 = 1;
            for (i, ln) in content_lines.iter().enumerate() {
                let mut w = ln.width() as u16;
                if i == 0 {
                    w = w.saturating_add(prefix_width);
                }
                if w > content_max_w {
                    content_max_w = w;
                }
            }

            let width = content_max_w
                .saturating_add(self.layout_padding * 2)
                .saturating_add(self.layout_thickness * 2)
                .clamp(3, max_width);

            let mut height = content_height
                .saturating_add(self.layout_padding * 2)
                .saturating_add(self.layout_thickness * 2)
                .max(3);
            height = height.min(config.max_height.max(3));

            let (x, y) = match config.position {
                NotificationPosition::TopLeft => (viewport.x + 2, viewport.y + y_offset + 1),
                NotificationPosition::TopCenter => (
                    viewport.x + (viewport.width.saturating_sub(width)) / 2,
                    viewport.y + y_offset + 1,
                ),
                NotificationPosition::TopRight => (
                    viewport.x + viewport.width.saturating_sub(width + 2),
                    viewport.y + y_offset + 1,
                ),
                NotificationPosition::BottomLeft => (
                    viewport.x + 2,
                    viewport.y + viewport.height.saturating_sub(height + y_offset + 1),
                ),
                NotificationPosition::BottomCenter => (
                    viewport.x + (viewport.width.saturating_sub(width)) / 2,
                    viewport.y + viewport.height.saturating_sub(height + y_offset + 1),
                ),
                NotificationPosition::BottomRight => (
                    viewport.x + viewport.width.saturating_sub(width + 2),
                    viewport.y + viewport.height.saturating_sub(height + y_offset + 1),
                ),
            };

            areas.push(Rect::new(x, y, width, height));
            y_offset += height + spacing;
        }

        for (item, area) in self.notifications.iter_mut().zip(areas.iter()) {
            item.area = *area;
        }
    }

    fn render_surface(
        &mut self,
        area: Rect,
        surface: &mut crate::render::CellSurface,
        cx: &RenderContext,
    ) {
        let config = cx.config();
        let config = &*config;
        self.update(
            config,
            cx.notification_history(),
            cx.work(),
            cx.ingress.clone(),
        );
        let notifications_config = &config.notifications;

        self.layout_thickness = if config.gradient_borders.enable {
            config.gradient_borders.thickness as u16
        } else {
            notifications_config.border.width as u16
        };
        self.layout_rounded = config.rounded_corners || notifications_config.border.radius > 0;
        self.layout_padding = notifications_config.padding;

        if self.notifications.is_empty() {
            return;
        }

        self.calculate_notification_areas(area, notifications_config);

        let mut model = NotificationModel::collect(self, config, cx.theme());
        let items: Vec<NotificationRenderItem> = model.items.clone();
        for item in items.iter().rev() {
            render_notification(&mut model, item, surface);
        }
    }
}

impl Component for NotificationPopup {
    fn handle_event(&mut self, _event: &Event, _cx: &mut Context) -> EventResult {
        EventResult::Ignored(None)
    }

    fn render(&mut self, area: Rect, surface: &mut crate::render::CellSurface, cx: &RenderContext) {
        self.render_surface(area, surface, cx);
    }
}

impl Default for NotificationPopup {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Owned snapshot model — Send + 'static, fully self-contained
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct NotificationRenderItem {
    message: String,
    severity: Severity,
    #[allow(dead_code)]
    id: usize,
    area: Rect,
    fade_progress: f32,
    corner_radius: u8,
    /// Fraction of the notification's auto-dismiss window still
    /// remaining: `1.0` immediately after the notification appears,
    /// trending to `0.0` as the timeout approaches, `None` when the
    /// notification has no timeout (sticky / user-dismiss only).
    /// Renderer draws this as a thin progress bar at the bottom of
    /// the popup so users see when the popup will disappear.
    dismiss_remaining: Option<f32>,
}

#[derive(Clone, Copy)]
struct NotificationStyles {
    popup: Style,
    border: Style,
    error: Style,
    warning: Style,
    info: Style,
    hint: Style,
}

impl NotificationStyles {
    fn for_severity(&self, severity: &Severity, fade_progress: f32) -> Style {
        let base = match severity {
            Severity::Error => self.error,
            Severity::Warning => self.warning,
            Severity::Info => self.info,
            Severity::Hint => self.hint,
        };
        if fade_progress > 0.5 {
            base.fg(Color::Gray)
        } else {
            base
        }
    }
}

/// Fully owned notification render model — `Send + 'static`.
#[derive(Clone)]
pub struct NotificationModel {
    items: Vec<NotificationRenderItem>,
    styles: NotificationStyles,
    show_emojis: bool,
    show_icons: bool,
    emojis: NotificationEmojis,
    icons: NotificationIcons,
    padding: u16,
    shadow: NotificationShadowConfig,
    border_enable: bool,
    border_width: u8,
    gradient_enable: bool,
    gradient_thickness: u16,
    rounded_corners: bool,
    gradient_border: Option<GradientBorder>,
}

impl NotificationModel {
    fn collect(popup: &NotificationPopup, config: &Config, theme: &Theme) -> Self {
        let notifications_config = &config.notifications;

        let items: Vec<NotificationRenderItem> = popup
            .notifications
            .iter()
            .map(|item| {
                // Compute the remaining-time fraction so the renderer
                // can draw a thin progress bar at the bottom edge —
                // visible cue for "how long until this disappears".
                // None when the notification doesn't auto-dismiss.
                let dismiss_remaining = item.notification.timeout.map(|timeout| {
                    let elapsed = item.notification.timestamp.elapsed();
                    if elapsed >= timeout {
                        0.0
                    } else {
                        let total_ms = timeout.as_millis() as f32;
                        let remaining_ms = (timeout - elapsed).as_millis() as f32;
                        // Clamp defensively — `Duration::saturating_sub` can
                        // give a non-negative result that still rounds out of
                        // range under unusual clock conditions.
                        (remaining_ms / total_ms).clamp(0.0, 1.0)
                    }
                });
                NotificationRenderItem {
                    message: item.notification.message.to_string(),
                    severity: item.notification.severity,
                    id: item.notification.id,
                    area: item.area,
                    fade_progress: item.fade_progress(),
                    corner_radius: item
                        .notification
                        .corner_radius
                        .unwrap_or(notifications_config.border.radius),
                    dismiss_remaining,
                }
            })
            .collect();

        let styles = NotificationStyles {
            popup: theme.get("ui.popup"),
            border: theme.get("ui.popup.border"),
            error: theme.get("error"),
            warning: theme.get("warning"),
            info: theme.get("info"),
            hint: theme.get("hint"),
        };

        let gradient_border =
            if notifications_config.border.enable && config.gradient_borders.enable {
                let mut gb = popup
                    .gradient_border
                    .clone()
                    .unwrap_or_else(|| GradientBorder::from_theme(theme, &config.gradient_borders));
                gb.disable_animation();
                Some(gb)
            } else {
                None
            };

        Self {
            items,
            styles,
            show_emojis: notifications_config.show_emojis,
            show_icons: notifications_config.show_icons,
            emojis: notifications_config.emojis.clone(),
            icons: notifications_config.icons.clone(),
            padding: notifications_config.padding,
            shadow: notifications_config.shadow.clone(),
            border_enable: notifications_config.border.enable,
            border_width: notifications_config.border.width,
            gradient_enable: config.gradient_borders.enable,
            gradient_thickness: config.gradient_borders.thickness as u16,
            rounded_corners: config.rounded_corners,
            gradient_border,
        }
    }

    fn cache_id() -> crate::render::CacheId {
        crate::render::CacheId::hashed(&"notification_popup")
    }

    fn cache_key(&self) -> crate::render::CacheKey {
        use std::collections::hash_map::DefaultHasher;
        let mut h = DefaultHasher::new();
        self.items.len().hash(&mut h);
        for item in &self.items {
            item.id.hash(&mut h);
            item.area.hash(&mut h);
            item.message.hash(&mut h);
            std::mem::discriminant(&item.severity).hash(&mut h);
            item.fade_progress.to_bits().hash(&mut h);
            item.corner_radius.hash(&mut h);
            // Quantize the dismiss-remaining fraction to 32 buckets
            // so the cache invalidates a couple of dozen times across
            // a typical dismiss window — frequent enough that the
            // progress bar visibly moves, infrequent enough that we
            // don't redraw every frame for an idle popup.
            item.dismiss_remaining
                .map(|f| (f * 32.0) as u8)
                .hash(&mut h);
        }
        self.show_emojis.hash(&mut h);
        self.show_icons.hash(&mut h);
        self.padding.hash(&mut h);
        self.border_enable.hash(&mut h);
        self.border_width.hash(&mut h);
        self.gradient_enable.hash(&mut h);
        self.gradient_thickness.hash(&mut h);
        self.rounded_corners.hash(&mut h);
        crate::render::CacheKey::hashed(&h.finish())
    }
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Renderer — produces PreparedRender directly via snapshot pattern.
// ---------------------------------------------------------------------------

fn prepare_notification_render(
    model: NotificationModel,
    area: Rect,
) -> crate::render::PreparedRender {
    use crate::render::{CacheTag, PreparedRender, RenderOutput};

    let tag = CacheTag {
        id: NotificationModel::cache_id(),
        key: model.cache_key(),
        area,
    };
    PreparedRender::snapshot(tag, model, move |mut model| {
        let mut output = RenderOutput::new(area);
        let items: Vec<NotificationRenderItem> = model.items.clone();
        for item in items.iter().rev() {
            render_notification(&mut model, item, output.surface_mut());
        }
        output
    })
}

/// Render a single notification item. Takes `&mut NotificationModel` so the
/// gradient border (if any) can be mutated during rendering.
fn render_notification(
    model: &mut NotificationModel,
    item: &NotificationRenderItem,
    surface: &mut crate::render::CellSurface,
) {
    if item.area.width < 4 || item.area.height < 3 {
        return;
    }

    // Optional drop shadow
    if model.shadow.enable && item.area.width > 2 && item.area.height > 2 {
        let shadow_area = Rect {
            x: item.area.x.saturating_add(model.shadow.offset_x),
            y: item.area.y.saturating_add(model.shadow.offset_y),
            width: item.area.width,
            height: item.area.height,
        };
        let shadow = model
            .styles
            .popup
            .bg(Color::Rgb(0, 0, 0))
            .add_modifier(Modifier::DIM);
        {
            let area = tui::ratatui::to_ratatui_rect(shadow_area);
            tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
            surface.set_style(area, tui::ratatui::to_ratatui_style(shadow));
        };
    }

    let notification_style = model
        .styles
        .for_severity(&item.severity, item.fade_progress);
    let rounded = model.rounded_corners || item.corner_radius > 0;

    // Render border and compute inner area
    let inner_area = if model.border_enable {
        if model.gradient_enable {
            if let Some(ref mut gb) = model.gradient_border {
                gb.render_no_theme(item.area, surface, rounded);
            }
            let t = model.gradient_thickness;
            Rect {
                x: item.area.x + t,
                y: item.area.y + t,
                width: item.area.width.saturating_sub(t * 2),
                height: item.area.height.saturating_sub(t * 2),
            }
        } else {
            render_simple_border(
                item.area,
                surface,
                model.styles.border,
                rounded,
                model.border_width,
            );
            let bw = model.border_width as u16;
            Rect {
                x: item.area.x + bw,
                y: item.area.y + bw,
                width: item.area.width.saturating_sub(bw * 2),
                height: item.area.height.saturating_sub(bw * 2),
            }
        }
    } else {
        item.area
    };

    // Content area with padding
    let mut content_area = inner_area;
    let pad = model.padding;
    if content_area.width > pad * 2 && content_area.height > pad * 2 {
        content_area = Rect {
            x: content_area.x + pad,
            y: content_area.y + pad,
            width: content_area.width - pad * 2,
            height: content_area.height - pad * 2,
        };
    }
    if content_area.width == 0 {
        content_area.width = 1;
    }
    if content_area.height == 0 {
        content_area.height = 1;
    }

    // Fill background
    if inner_area.width > 0 && inner_area.height > 0 {
        {
            let area = tui::ratatui::to_ratatui_rect(inner_area);
            tui::ratatui::widgets::Widget::render(tui::ratatui::widgets::Clear, area, surface);
            surface.set_style(area, tui::ratatui::to_ratatui_style(model.styles.popup));
        };
    }

    // Render text content
    let content_lines = wrap_text(&item.message, content_area.width.max(1));

    let mut prefix = String::new();
    if model.show_emojis {
        prefix.push_str(notification_emoji(&item.severity, &model.emojis));
        prefix.push(' ');
    } else if model.show_icons {
        prefix.push_str(notification_icon(&item.severity, &model.icons));
        prefix.push(' ');
    }

    let prefix_width = prefix.width() as u16;
    let show_prefix = !prefix.is_empty() && content_area.width > prefix_width + 1;

    for (y_pos, (i, line)) in (content_area.y..).zip(content_lines.iter().enumerate()) {
        if y_pos >= content_area.y + content_area.height {
            break;
        }

        if i == 0 && show_prefix {
            surface.set_string(
                content_area.x,
                y_pos,
                &prefix,
                tui::ratatui::to_ratatui_style(notification_style),
            );
        }

        let x_offset = if i == 0 && show_prefix {
            prefix_width
        } else {
            0
        };
        let available = content_area.width.saturating_sub(x_offset).max(1) as usize;
        surface.set_stringn(
            content_area.x + x_offset,
            y_pos,
            line,
            available,
            tui::ratatui::to_ratatui_style(notification_style),
        );
    }

    // Auto-dismiss countdown bar — a single row of fractional block
    // glyphs along the *bottom edge* of the inner area, draining
    // from full to empty as the dismiss deadline approaches. We
    // skip the bar entirely when the notification doesn't auto-
    // dismiss (`dismiss_remaining` is None) so sticky notifications
    // don't get a permanently-full progress bar that misleads about
    // their state. Severity color drives the bar tint so error bars
    // read red, warnings yellow, etc. — same visual language as the
    // text content.
    if let Some(remaining) = item.dismiss_remaining {
        // Bar lives on the bottom-most row inside the border. Skip
        // when the inner area is too short to spare a row for it.
        if inner_area.height >= 2 && inner_area.width >= 2 {
            let bar_y = inner_area.y + inner_area.height.saturating_sub(1);
            let bar_x = inner_area.x;
            // Inset 1 cell on each side so the bar doesn't bleed into
            // the rounded border glyphs at the corners.
            let bar_width = inner_area.width.saturating_sub(2);
            if bar_width > 0 {
                let total_cells = bar_width as f32;
                let full_cells = (remaining * total_cells).floor();
                let partial_eighth = ((remaining * total_cells - full_cells) * 8.0).round() as u8;
                let full = full_cells as u16;
                let bar_style = notification_style;
                for i in 0..bar_width {
                    let glyph = if i < full {
                        "█"
                    } else if i == full && partial_eighth > 0 {
                        // Eighth-block fractional glyphs (▏▎▍▌▋▊▉) for
                        // sub-cell smoothness — the eye reads movement
                        // even on slow countdowns.
                        match partial_eighth {
                            1 => "▏",
                            2 => "▎",
                            3 => "▍",
                            4 => "▌",
                            5 => "▋",
                            6 => "▊",
                            7 => "▉",
                            _ => "█",
                        }
                    } else {
                        " "
                    };
                    surface.set_stringn(
                        bar_x + 1 + i,
                        bar_y,
                        glyph,
                        1,
                        tui::ratatui::to_ratatui_style(bar_style),
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn notification_icon<'a>(severity: &Severity, config: &'a NotificationIcons) -> &'a str {
    match severity {
        Severity::Error => &config.error,
        Severity::Warning => &config.warning,
        Severity::Info | Severity::Hint => &config.info,
    }
}

fn notification_emoji<'a>(severity: &Severity, config: &'a NotificationEmojis) -> &'a str {
    match severity {
        Severity::Error => &config.error,
        Severity::Warning => &config.warning,
        Severity::Info | Severity::Hint => &config.info,
    }
}

fn wrap_text(text: &str, max_width: u16) -> Vec<String> {
    let mut lines = Vec::new();
    let max_width = max_width as usize;

    for line in text.lines() {
        if line.width() <= max_width {
            lines.push(line.to_string());
        } else {
            let mut current_line = String::new();
            let mut current_width = 0;

            for word in line.split_whitespace() {
                let word_width = word.width();
                if current_width + word_width < max_width {
                    if !current_line.is_empty() {
                        current_line.push(' ');
                        current_width += 1;
                    }
                    current_line.push_str(word);
                    current_width += word_width;
                } else {
                    if !current_line.is_empty() {
                        lines.push(current_line);
                        current_line = String::new();
                        current_width = 0;
                    }
                    if word_width <= max_width {
                        current_line = word.to_string();
                        current_width = word_width;
                    } else {
                        let truncated = word
                            .chars()
                            .take(max_width.saturating_sub(3))
                            .collect::<String>()
                            + "...";
                        lines.push(truncated);
                    }
                }
            }
            if !current_line.is_empty() {
                lines.push(current_line);
            }
        }
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

fn render_simple_border(
    area: Rect,
    surface: &mut crate::render::CellSurface,
    style: Style,
    rounded: bool,
    width: u8,
) {
    let (h, v, tl, tr, bl, br) = if rounded {
        ("─", "│", "╭", "╮", "╰", "╯")
    } else {
        ("─", "│", "┌", "┐", "└", "┘")
    };

    let w = width.max(1) as u16;
    for s in 0..w {
        let x0 = area.x.saturating_add(s);
        let x1 = area.right().saturating_sub(1 + s);
        let y0 = area.y.saturating_add(s);
        let y1 = area.bottom().saturating_sub(1 + s);

        if x0 >= x1 || y0 >= y1 {
            break;
        }

        for x in x0..=x1 {
            let ch_top = if x == x0 {
                tl
            } else if x == x1 {
                tr
            } else {
                h
            };
            let ch_bot = if x == x0 {
                bl
            } else if x == x1 {
                br
            } else {
                h
            };
            {
                if let Some(cell) = surface.cell_mut((x, y0)) {
                    cell.set_symbol(ch_top);
                    cell.set_style(tui::ratatui::to_ratatui_style(style));
                }
            };
            {
                if let Some(cell) = surface.cell_mut((x, y1)) {
                    cell.set_symbol(ch_bot);
                    cell.set_style(tui::ratatui::to_ratatui_style(style));
                }
            };
        }
        for y in (y0 + 1)..y1 {
            {
                if let Some(cell) = surface.cell_mut((x0, y)) {
                    cell.set_symbol(v);
                    cell.set_style(tui::ratatui::to_ratatui_style(style));
                }
            };
            {
                if let Some(cell) = surface.cell_mut((x1, y)) {
                    cell.set_symbol(v);
                    cell.set_style(tui::ratatui::to_ratatui_style(style));
                }
            };
        }
    }
}
