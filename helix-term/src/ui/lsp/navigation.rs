//! LSP goto / jump (compositor + editor). Ingress types: [`crate::runtime::ui::command::LspLocation`].

use helix_lsp::{lsp, OffsetEncoding};
use helix_stdx::env;
use helix_view::editor::Action;
use helix_view::view::push_jump;
use helix_view::Editor;

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

pub(crate) fn jump_to_location(
    editor: &mut Editor,
    ingress: &crate::runtime::RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
    location: &LspLocation,
    action: Action,
) {
    let (view_id, doc) = focused!(editor);
    let view = view_mut!(editor, view_id);
    push_jump(view, doc);

    let Some(path) = location.uri.as_path() else {
        let err = format!("unable to convert URI to filepath: {:?}", location.uri);
        editor.set_error(err);
        return;
    };
    crate::runtime::ui::document::queue_document_open(
        editor,
        ingress,
        foreground,
        crate::runtime::DocumentOpenRequest {
            path: path.to_path_buf(),
            action,
            lane: crate::runtime::DocumentOpenLane::Navigation,
            target: crate::runtime::DocumentOpenTarget::View(view_id),
            selection: crate::runtime::DocumentOpenSelection::LspRange {
                range: location.range,
                offset_encoding: location.offset_encoding,
            },
            alignment: crate::runtime::DocumentOpenAlignment::CenterIfAction,
            default_folding_if_new: false,
            fff_record: None,
            external_if_binary: None,
            post_action: crate::runtime::DocumentOpenPostAction::None,
            completion: crate::runtime::DocumentOpenCompletionTarget::Editor,
        },
    );
}

/// Picker or single-file jump. Call only when `locations` is non-empty, or rely on [`crate::runtime::ui::apply`] empty handling for [`crate::runtime::ui::command::LspCommand::Goto`].
pub(crate) fn goto_locations(
    editor: &mut Editor,
    compositor: &mut Compositor,
    ingress: crate::runtime::RuntimeIngress,
    foreground: &crate::runtime::ForegroundEvents,
    locations: Vec<LspLocation>,
) {
    let cwdir = env::current_working_dir();

    match locations.as_slice() {
        [location] => {
            jump_to_location(editor, &ingress, foreground, location, Action::Replace);
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
                crate::ui::PickerRuntime::new(editor),
                ingress,
                |cx: &mut crate::compositor::Context, location, action| {
                    jump_to_location(cx.editor, &cx.ingress, &cx.foreground, location, action);
                },
            )
            .with_preview(|_editor, location| location_to_file_location(location));
            compositor.push(Box::new(overlaid(picker)));
        }
    }
}
