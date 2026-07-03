use std::collections::{BTreeMap, HashMap};

use helix_pkg::{OpEvent, Ops, PkgKind, Receipt};
use helix_view::graphics::Rect;
use tui::text::Span;

use crate::ui::PickerColumn;

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
            OpEvent::Progress { name, message } => {
                self.active.insert(
                    name.clone(),
                    PkgStatusline {
                        label: format!("pkg {name}: {message}"),
                        percent: parse_percent(message),
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

#[derive(Debug, Clone)]
pub struct PkgPickerItem {
    pub name: String,
    pub kind: PkgKind,
    pub installed: Option<String>,
    pub latest: String,
    pub languages: String,
}

pub type PkgPicker = crate::ui::Picker<PkgPickerItem, ()>;

pub fn picker(
    editor: &helix_view::Editor,
    ingress: crate::runtime::RuntimeIngress,
) -> anyhow::Result<PkgPicker> {
    let ops = Ops::open_default()?;
    let receipts: HashMap<(PkgKind, String), Receipt> = ops
        .store()
        .receipts()?
        .into_iter()
        .map(|receipt| ((receipt.kind, receipt.name.clone()), receipt))
        .collect();

    let mut entries: Vec<_> = ops
        .registry()
        .iter()
        .map(|package| {
            let installed = receipts
                .get(&(package.kind, package.name.clone()))
                .map(|receipt| receipt.version.clone());
            PkgPickerItem {
                name: package.name.clone(),
                kind: package.kind,
                installed,
                latest: package
                    .version
                    .tag_source
                    .as_deref()
                    .unwrap_or("registry")
                    .to_owned(),
                languages: package.languages.join(","),
            }
        })
        .collect();
    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.cmp(&right.name))
    });

    let columns = [
        PickerColumn::new("name", |item: &PkgPickerItem, _: &()| {
            Span::raw(item.name.as_str()).into()
        }),
        PickerColumn::new("kind", |item: &PkgPickerItem, _: &()| {
            Span::raw(item.kind.to_string()).into()
        })
        .without_filtering(),
        PickerColumn::new("installed", |item: &PkgPickerItem, _: &()| {
            Span::raw(item.installed.as_deref().unwrap_or("-")).into()
        })
        .without_filtering(),
        PickerColumn::new("latest", |item: &PkgPickerItem, _: &()| {
            Span::raw(item.latest.as_str()).into()
        })
        .without_filtering(),
        PickerColumn::new("languages", |item: &PkgPickerItem, _: &()| {
            Span::raw(item.languages.as_str()).into()
        }),
    ];

    let install = ingress.clone();
    let update = ingress.clone();
    let remove = ingress.clone();
    let doctor = ingress.clone();

    let mut handlers = crate::ui::picker::PickerKeyHandlers::new();
    handlers.insert(
        helix_view::input::KeyEvent {
            code: helix_view::input::KeyCode::Char('u'),
            modifiers: helix_view::input::KeyModifiers::NONE,
        },
        Box::new(move |cx, item: &PkgPickerItem, _data, _cursor| {
            crate::runtime::spawn_pkg_operation(
                crate::runtime::PkgOperation::Update(vec![item.name.clone()]),
                cx.editor.work(),
                update.clone(),
            );
        }),
    );
    handlers.insert(
        helix_view::input::KeyEvent {
            code: helix_view::input::KeyCode::Char('d'),
            modifiers: helix_view::input::KeyModifiers::NONE,
        },
        Box::new(move |cx, item: &PkgPickerItem, _data, _cursor| {
            if item.installed.is_none() {
                cx.editor
                    .set_status(format!("{} is not installed", item.name));
                return;
            }
            crate::runtime::spawn_pkg_operation(
                crate::runtime::PkgOperation::Remove(vec![item.name.clone()]),
                cx.editor.work(),
                remove.clone(),
            );
        }),
    );
    handlers.insert(
        helix_view::input::KeyEvent {
            code: helix_view::input::KeyCode::Char('!'),
            modifiers: helix_view::input::KeyModifiers::NONE,
        },
        Box::new(move |cx, _item: &PkgPickerItem, _data, _cursor| {
            crate::runtime::spawn_pkg_operation(
                crate::runtime::PkgOperation::Doctor,
                cx.editor.work(),
                doctor.clone(),
            );
        }),
    );

    Ok(crate::ui::Picker::new(
        columns,
        0,
        entries,
        (),
        crate::ui::PickerRuntime::new(editor),
        ingress,
        move |cx, item: &PkgPickerItem, _action| {
            crate::runtime::spawn_pkg_operation(
                crate::runtime::PkgOperation::Install(vec![item.name.clone()]),
                cx.editor.work(),
                install.clone(),
            );
        },
    )
    .with_multi_select()
    .show_preview(false)
    .with_key_handlers(handlers)
    .with_custom_hints([
        crate::widgets::Hint::new("Enter", "install").priority(220),
        crate::widgets::Hint::new("u", "update").priority(205),
        crate::widgets::Hint::new("d", "remove").priority(204),
        crate::widgets::Hint::new("!", "doctor").priority(203),
    ]))
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

fn parse_percent(message: &str) -> Option<u8> {
    let number = message
        .split(|ch: char| !ch.is_ascii_digit())
        .find(|part| !part.is_empty())?
        .parse::<u8>()
        .ok()?;
    (number <= 100).then_some(number)
}

#[allow(dead_code)]
pub fn statusline_rect(view: &helix_view::View) -> Rect {
    view.area.clip_top(view.area.height.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkg_picker_constructs_headlessly() {
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

            let _picker = picker(&editor, ingress).expect("pkg picker opens");
        });
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
        });
        assert_eq!(state.statusline().unwrap().percent, Some(42));

        state.apply(&OpEvent::Done {
            name: "rust-analyzer".into(),
        });
        assert_eq!(state.statusline(), None);
    }
}
