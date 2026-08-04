use crate::{
    compositor::Compositor,
    runtime::ui::command::CodeAtlasCommand,
    ui::code_atlas::{CodeAtlasSession, ID},
};
use helix_view::{editor::Action, tree, Editor};

pub(crate) fn apply_code_atlas_command(
    editor: &mut Editor,
    compositor: &mut Compositor,
    _ingress: crate::runtime::RuntimeIngress,
    command: CodeAtlasCommand,
) {
    if let Some(mut previous) = compositor.remove_by_id(ID) {
        if let Some(session) = previous.downcast_mut::<CodeAtlasSession>() {
            session.dispose(editor, false);
        }
    }

    let CodeAtlasCommand::Open {
        briefing,
        source_document,
        source_view,
    } = command;
    let source_is_live = editor
        .tree
        .try_get(source_view)
        .is_some_and(|view| view.doc == source_document);
    if !source_is_live {
        editor.set_error("Could not open briefing: source view no longer exists");
        return;
    }
    let Some(source) = editor.document(source_document) else {
        editor.set_error("Could not open briefing: source document no longer exists");
        return;
    };
    let source_selection = source.selection(source_view).clone();
    let source_offset = source.view_offset(source_view);
    let source_version = source.version();
    let text = CodeAtlasSession::initial_text(&briefing);
    let name = format!("understand://{}", briefing.subject.label);
    let briefing_document = editor.open_named_scratch(Action::VerticalSplit, text, name, None);
    let briefing_view = editor.focused_view_id();
    editor.swap_split_in_direction(tree::Direction::Left);

    let mut session = CodeAtlasSession::new(
        source_document,
        source_view,
        source_selection,
        source_offset,
        source_version,
        briefing_document,
        briefing_view,
        briefing,
    );
    session.initialize(editor);
    compositor.push(Box::new(session));
}
