//! LSP goto / jump (compositor + editor). Ingress types: [`crate::runtime::ui::command::LspLocation`].

use std::path::Path;

use helix_core::Selection;
use helix_lsp::util::lsp_range_to_range;
use helix_lsp::{lsp, OffsetEncoding};
use helix_stdx::env;
use helix_view::editor::Action;
use helix_view::view::push_jump;
use helix_view::{align_view, Align, Editor};

use crate::compositor::Compositor;
use crate::runtime::ui::command::LspLocation;
use crate::ui::{self, overlay::overlaid, FileLocation, Picker};

pub(crate) fn lsp_location_to_lsp_location(
    location: lsp::Location,
    offset_encoding: OffsetEncoding,
) -> Option<LspLocation> {
    let uri = match location.uri.try_into() {
        Ok(uri) => uri,
        Err(err) => {
            log::warn!("discarding invalid or unsupported URI: {err}");
            return None;
        }
    };
    Some(LspLocation {
        uri,
        range: location.range,
        offset_encoding,
    })
}

pub(crate) fn location_to_file_location(location: &LspLocation) -> Option<FileLocation<'_>> {
    let path = location.uri.as_path()?;
    let line = Some((
        location.range.start.line as usize,
        location.range.end.line as usize,
    ));
    Some((path.into(), line))
}

pub(crate) fn jump_to_location(editor: &mut Editor, location: &LspLocation, action: Action) {
    let (view_id, doc) = focused!(editor);
    let view = view_mut!(editor, view_id);
    push_jump(view, doc);

    let Some(path) = location.uri.as_path() else {
        let err = format!("unable to convert URI to filepath: {:?}", location.uri);
        editor.set_error(err);
        return;
    };
    jump_to_position(
        editor,
        path,
        location.range,
        location.offset_encoding,
        action,
    );
}

fn jump_to_position(
    editor: &mut Editor,
    path: &Path,
    range: lsp::Range,
    offset_encoding: OffsetEncoding,
    action: Action,
) {
    let doc = match editor.open(path, action) {
        Ok(id) => doc_mut!(editor, &id),
        Err(err) => {
            let err = format!("failed to open path: {:?}: {:?}", path, err);
            editor.set_error(err);
            return;
        }
    };
    let view = view_mut!(editor);
    let new_range = if let Some(new_range) = lsp_range_to_range(doc.text(), range, offset_encoding)
    {
        new_range
    } else {
        log::warn!("lsp position out of bounds - {:?}", range);
        return;
    };
    doc.set_selection(view.id, Selection::single(new_range.head, new_range.anchor));
    if action.align_view(view, doc.id()) {
        align_view(doc, view, Align::Center);
    }
}

/// Picker or single-file jump. Call only when `locations` is non-empty, or rely on [`crate::runtime::ui::apply`] empty handling for [`crate::runtime::ui::command::LspCommand::Goto`].
pub(crate) fn goto_locations(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: helix_runtime::Sender<crate::runtime::RuntimeEvent>,
    locations: Vec<LspLocation>,
) {
    let cwdir = env::current_working_dir();

    match locations.as_slice() {
        [location] => {
            jump_to_location(editor, location, Action::Replace);
        }
        [] => {}
        _ => {
            let columns = [ui::PickerColumn::new(
                "location",
                |item: &LspLocation, cwdir: &std::path::PathBuf| {
                    let path = if let Some(path) = item.uri.as_path() {
                        path.strip_prefix(cwdir).unwrap_or(path).to_string_lossy()
                    } else {
                        item.uri.to_string().into()
                    };

                    format!("{path}:{}", item.range.start.line + 1).into()
                },
            )];

            let picker = Picker::new(
                columns,
                0,
                locations,
                cwdir,
                crate::ui::PickerRuntime::new(editor.runtime()),
                ingress,
                |cx: &mut crate::compositor::Context, location, action| {
                    jump_to_location(cx.editor, location, action);
                },
            )
            .with_preview(|_editor, location| location_to_file_location(location));
            compositor.push(Box::new(overlaid(picker)));
        }
    }
}
