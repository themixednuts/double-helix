use crate::compositor::{Component, Context, Event, EventResult, RenderContext};
use crate::runtime::{RuntimeEvent, RuntimeTaskEvent};
use crate::ui::gradient_border::GradientBorder;
use helix_core::unicode::width::UnicodeWidthStr;
use helix_runtime::Sender as IngressSender;
use helix_view::theme::Modifier;
use helix_view::{
    editor::{
        CmdlineStyle, Notification, NotificationConfig, NotificationEmojis, NotificationIcons,
        NotificationPosition, NotificationShadowConfig, NotificationStyle, Severity,
    },
    graphics::{Color, Rect, Style},
    Editor,
};
use std::hash::{Hash, Hasher};
use std::time::Instant;
use tokio::time::sleep as tokio_sleep;
use tui::buffer::Buffer as Surface;

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

    pub fn update(&mut self, editor: &Editor, ingress: IngressSender<RuntimeEvent>) {
        let config = &editor.config().notifications;

        if !config.enable
            || config.style != NotificationStyle::Popup
            || editor.config().cmdline.style != CmdlineStyle::Popup
        {
            self.notifications.clear();
            return;
        }

        let active_notifications = editor.get_active_notifications();

        self.notifications.retain_mut(|item| {
            let still_active = active_notifications
                .iter()
                .any(|n| n.id == item.notification.id);
            let is_expired = item.notification.is_expired();
            still_active && !is_expired
        });

        for notification in active_notifications {
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

                    editor
                        .work()
                        .spawn(async move {
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

    /// Sync state from editor and produce a [`crate::render::PreparedRender`]
    /// via owned snapshot.  Returns `None` when there are no notifications to
    /// draw — the caller should skip composition entirely.
    pub fn prepare_snapshot(
        &mut self,
        area: Rect,
        cx: &RenderContext,
    ) -> Option<crate::render::PreparedRender> {
        self.update(cx.editor, cx.ingress.clone());

        let editor = cx.editor;

        self.layout_thickness = if editor.config().gradient_borders.enable {
            editor.config().gradient_borders.thickness as u16
        } else {
            editor.config().notifications.border.width as u16
        };
        self.layout_rounded =
            editor.config().rounded_corners || editor.config().notifications.border.radius > 0;
        self.layout_padding = editor.config().notifications.padding;

        if self.notifications.is_empty() {
            return None;
        }

        let config = &editor.config().notifications;
        self.calculate_notification_areas(area, config);

        let model = NotificationModel::collect(self, editor);
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
}

impl Component for NotificationPopup {
    fn handle_event(&mut self, _event: &Event, _cx: &mut Context) -> EventResult {
        EventResult::Ignored(None)
    }

    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &RenderContext) {
        // Legacy eager path — kept for Component trait compliance.
        // EditorView now uses prepare_snapshot() directly.
        self.update(cx.editor, cx.ingress.clone());

        self.layout_thickness = if cx.editor.config().gradient_borders.enable {
            cx.editor.config().gradient_borders.thickness as u16
        } else {
            cx.editor.config().notifications.border.width as u16
        };
        self.layout_rounded = cx.editor.config().rounded_corners
            || cx.editor.config().notifications.border.radius > 0;
        self.layout_padding = cx.editor.config().notifications.padding;

        if self.notifications.is_empty() {
            return;
        }

        let config = &cx.editor.config().notifications;
        self.calculate_notification_areas(area, config);

        let mut model = NotificationModel::collect(self, cx.editor);
        let items: Vec<NotificationRenderItem> = model.items.clone();
        for item in items.iter().rev() {
            render_notification(&mut model, item, surface);
        }
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
    fn collect(popup: &NotificationPopup, editor: &Editor) -> Self {
        let config = &editor.config().notifications;
        let theme = &editor.theme;

        let items: Vec<NotificationRenderItem> = popup
            .notifications
            .iter()
            .map(|item| NotificationRenderItem {
                message: item.notification.message.to_string(),
                severity: item.notification.severity,
                id: item.notification.id,
                area: item.area,
                fade_progress: item.fade_progress(),
                corner_radius: item
                    .notification
                    .corner_radius
                    .unwrap_or(config.border.radius),
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

        let gradient_border = if config.border.enable && editor.config().gradient_borders.enable {
            let mut gb = popup.gradient_border.clone().unwrap_or_else(|| {
                GradientBorder::from_theme(theme, &editor.config().gradient_borders)
            });
            gb.disable_animation();
            Some(gb)
        } else {
            None
        };

        Self {
            items,
            styles,
            show_emojis: config.show_emojis,
            show_icons: config.show_icons,
            emojis: config.emojis.clone(),
            icons: config.icons.clone(),
            padding: config.padding,
            shadow: config.shadow.clone(),
            border_enable: config.border.enable,
            border_width: config.border.width,
            gradient_enable: editor.config().gradient_borders.enable,
            gradient_thickness: editor.config().gradient_borders.thickness as u16,
            rounded_corners: editor.config().rounded_corners,
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
        let mut surface = Surface::empty(area);
        let items: Vec<NotificationRenderItem> = model.items.clone();
        for item in items.iter().rev() {
            render_notification(&mut model, item, &mut surface);
        }
        RenderOutput { area, surface }
    })
}

/// Render a single notification item. Takes `&mut NotificationModel` so the
/// gradient border (if any) can be mutated during rendering.
fn render_notification(
    model: &mut NotificationModel,
    item: &NotificationRenderItem,
    surface: &mut Surface,
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
        surface.clear_with(shadow_area, shadow);
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
        surface.clear_with(inner_area, model.styles.popup);
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
            surface.set_string(content_area.x, y_pos, &prefix, notification_style);
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
            notification_style,
        );
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

fn render_simple_border(area: Rect, surface: &mut Surface, style: Style, rounded: bool, width: u8) {
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
            if let Some(cell) = surface.get_mut(x, y0) {
                cell.set_symbol(ch_top).set_style(style);
            }
            if let Some(cell) = surface.get_mut(x, y1) {
                cell.set_symbol(ch_bot).set_style(style);
            }
        }
        for y in (y0 + 1)..y1 {
            if let Some(cell) = surface.get_mut(x0, y) {
                cell.set_symbol(v).set_style(style);
            }
            if let Some(cell) = surface.get_mut(x1, y) {
                cell.set_symbol(v).set_style(style);
            }
        }
    }
}
